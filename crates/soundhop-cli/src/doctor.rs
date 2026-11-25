//! `soundhop doctor` and `soundhop bug-report` diagnostic commands.

use std::sync::Arc;

use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::Controller;
use soundhop_core::firewall::{
    detect_for_ports, open_ports, FirewallPorts, FirewallResult, FirewallState,
};
use soundhop_core::ipc::is_running as ipc_is_running;
use soundhop_core::service::{install as install_service, status as service_status};
use soundhop_core::util::{config_path, local_addresses, log_path, state_path};

use crate::ui::{error, header, info, lock_or_recover, ok, print_json, safe_print, warning};

#[cfg(any(target_os = "linux", test))]
fn parse_appimage_mounts(content: &str) -> Vec<String> {
    let mut mounts = Vec::new();
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
    mounts
}

#[cfg(target_os = "linux")]
fn list_appimage_mounts() -> Vec<String> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|content| parse_appimage_mounts(&content))
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn unmount_fuse(mount_point: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("fusermount");
    cmd.args(["-u", mount_point]);
    soundhop_core::util::close_fds_pre_exec(&mut cmd);
    let res = match cmd.output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut cmd3 = std::process::Command::new("fusermount3");
            cmd3.args(["-u", mount_point]);
            soundhop_core::util::close_fds_pre_exec(&mut cmd3);
            cmd3.output()
        }
        res => res,
    };
    match res {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                format!("Failed to unmount stale AppImage mount {mount_point}")
            } else {
                format!("Failed to unmount stale AppImage mount {mount_point}: {stderr}")
            })
        }
        Err(e) => Err(format!(
            "Failed to run fusermount/fusermount3 -u {mount_point}: {e}"
        )),
    }
}

trait DoctorEnv {
    fn detect_firewall(&self, ports: &FirewallPorts) -> FirewallState;
    fn open_firewall(&self, ports: &FirewallPorts) -> FirewallResult;
    fn service_status(&self) -> serde_json::Value;
    fn install_service(&self) -> serde_json::Value;
    fn ipc_running(&self) -> bool;
    fn local_addresses(&self) -> Vec<String>;
    fn list_appimage_mounts(&self) -> Vec<String>;
    fn own_appimage_mount(&self) -> Option<String>;
    fn unmount_fuse(&self, mount_point: &str) -> Result<(), String>;
}

struct SystemDoctorEnv;

impl DoctorEnv for SystemDoctorEnv {
    fn detect_firewall(&self, ports: &FirewallPorts) -> FirewallState {
        detect_for_ports(ports)
    }

    fn open_firewall(&self, ports: &FirewallPorts) -> FirewallResult {
        open_ports(ports, false)
    }

    fn service_status(&self) -> serde_json::Value {
        service_status()
    }

    fn install_service(&self) -> serde_json::Value {
        install_service(true, true, true)
    }

    fn ipc_running(&self) -> bool {
        ipc_is_running(None)
    }

    fn local_addresses(&self) -> Vec<String> {
        local_addresses(false)
    }

    fn list_appimage_mounts(&self) -> Vec<String> {
        #[cfg(target_os = "linux")]
        {
            list_appimage_mounts()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
    }

    fn own_appimage_mount(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            std::env::var_os("APPDIR")
                .map(|v| v.to_string_lossy().into_owned())
                .filter(|s| s.starts_with("/tmp/.mount_SoundH"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn unmount_fuse(&self, mount_point: &str) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            unmount_fuse(mount_point)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = mount_point;
            Ok(())
        }
    }
}

struct DoctorOutcome {
    report: serde_json::Value,
    exit_ok: bool,
    backend_ok: bool,
    server_name: String,
    is_pw: bool,
    def_sink: String,
    addrs: Vec<String>,
    fw: FirewallState,
    svc: serde_json::Value,
    svc_active: bool,
    daemon_ipc: bool,
    tools: serde_json::Value,
    appimage_mounts: Vec<String>,
    fixed_actions: Vec<String>,
    fix_errors: Vec<String>,
    daemon_log_path: std::path::PathBuf,
}

fn collect_doctor_report(
    controller: &Arc<Controller>,
    fix: bool,
    env: &dyn DoctorEnv,
) -> DoctorOutcome {
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

    let fw_ports = FirewallPorts::from_config(&cfg);
    let mut fw = env.detect_firewall(&fw_ports);
    if fix && fw.active {
        let fw_res = env.open_firewall(&fw_ports);
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
        fw = env.detect_firewall(&fw_ports);
    }

    let mut svc = env.service_status();
    let mut svc_active = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut daemon_ipc = env.ipc_running();
    if fix && !svc_active && !daemon_ipc {
        let svc_res = env.install_service();
        let enabled = svc_res
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let started = svc_res
            .get("started")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if started {
            #[cfg(windows)]
            fixed_actions.push(
                "Installed Windows HKCU Run autostart entry and started background tray"
                    .to_string(),
            );
            #[cfg(not(windows))]
            fixed_actions.push("Installed and started user service".to_string());
        } else {
            fix_failed = true;
            #[cfg(windows)]
            fix_errors.push(format!(
                "Failed to install/start Windows startup entry (enabled={enabled}, started={started})"
            ));
            #[cfg(not(windows))]
            fix_errors.push(format!(
                "Failed to install/start user service (enabled={enabled}, started={started})"
            ));
        }
        svc = env.service_status();
        svc_active = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        daemon_ipc = env.ipc_running();
    }

    let mut appimage_mounts: Vec<String> = env.list_appimage_mounts();
    if fix && appimage_mounts.len() > 1 {
        let own_mount = env.own_appimage_mount();
        for m in appimage_mounts
            .iter()
            .filter(|m| own_mount.as_deref() != Some(m.as_str()))
        {
            match env.unmount_fuse(m) {
                Ok(()) => {
                    fixed_actions.push(format!("Unmounted stale AppImage mount {m}"));
                }
                Err(err_msg) => {
                    fix_failed = true;
                    fix_errors.push(err_msg);
                }
            }
        }
        appimage_mounts = env.list_appimage_mounts();
    }

    let addrs = env.local_addresses();
    let tools = controller.android_tool_status();
    let daemon_log_path = log_path();
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
        "log_path": daemon_log_path.to_string_lossy(),
        "appimage_mounts": appimage_mounts,
        "daemon_ipc_running": daemon_ipc,
        "android_tools": tools,
        "fixed_actions": fixed_actions,
        "fix_errors": fix_errors,
        "healthy": backend_ok && !addrs.is_empty() && !fix_failed,
    });

    DoctorOutcome {
        report,
        exit_ok,
        backend_ok,
        server_name,
        is_pw,
        def_sink,
        addrs,
        fw,
        svc,
        svc_active,
        daemon_ipc,
        tools,
        appimage_mounts,
        fixed_actions,
        fix_errors,
        daemon_log_path,
    }
}

fn collect_bug_report(controller: &Arc<Controller>, env: &dyn DoctorEnv) -> serde_json::Value {
    let cfg = lock_or_recover(&controller.config).clone();
    let st = controller.status(None);
    let fw = env.detect_firewall(&FirewallPorts::from_config(&cfg));
    let svc = env.service_status();
    serde_json::json!({
        "version": VERSION,
        "protocol": PROTOCOL_VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "config_path": config_path().to_string_lossy(),
        "state_path": state_path().to_string_lossy(),
        "log_path": log_path().to_string_lossy(),
        "status": st,
        "firewall": fw,
        "service": svc,
    })
}

pub fn run_doctor(controller: &Arc<Controller>, fix: bool, json_out: bool) -> i32 {
    let outcome = collect_doctor_report(controller, fix, &SystemDoctorEnv);

    if json_out {
        print_json(&outcome.report);
        return if outcome.exit_ok { 0 } else { 1 };
    }

    header(&format!("SoundHop v{VERSION} System Diagnostics"));
    if outcome.backend_ok {
        #[cfg(windows)]
        {
            let _ = outcome.is_pw;
            if outcome.server_name.to_ascii_lowercase().contains("wasapi") {
                ok(&format!(
                    "Audio Backend: {} (Default Output: {})",
                    outcome.server_name, outcome.def_sink
                ));
            } else {
                ok(&format!(
                    "Audio Backend: {} (WASAPI, Default Output: {})",
                    outcome.server_name, outcome.def_sink
                ));
            }
        }
        #[cfg(not(windows))]
        ok(&format!(
            "Audio Backend: {} (PipeWire: {}, Default Sink: {})",
            outcome.server_name, outcome.is_pw, outcome.def_sink
        ));
    } else {
        #[cfg(windows)]
        error("Audio Backend: WASAPI unavailable (verify the Windows Audio service is running and an output device is enabled in Sound Settings)");
        #[cfg(not(windows))]
        error("Audio Backend: unreachable (install pipewire-pulse or pulseaudio-utils)");
    }

    if outcome.addrs.is_empty() {
        warning("Network: No private LAN IPv4 addresses detected.");
    } else {
        ok(&format!("LAN Addresses: {}", outcome.addrs.join(", ")));
    }

    if outcome.fw.active {
        #[cfg(windows)]
        info(&format!(
            "Windows Firewall: {} ({}; run `soundhop firewall open` or `soundhop doctor --fix` to allow LAN audio)",
            outcome.fw.kind, outcome.fw.detail
        ));
        #[cfg(not(windows))]
        info(&format!(
            "Firewall: {} ({})",
            outcome.fw.kind, outcome.fw.detail
        ));
    } else {
        #[cfg(windows)]
        ok(&format!(
            "Windows Firewall: {} ({})",
            outcome.fw.kind, outcome.fw.detail
        ));
        #[cfg(not(windows))]
        ok(&format!(
            "Firewall: {} ({})",
            outcome.fw.kind, outcome.fw.detail
        ));
    }

    if outcome.daemon_ipc || outcome.svc_active {
        ok("Background Daemon: Running");
    } else {
        #[cfg(windows)]
        warning("Background Daemon: Not running (run `soundhop-gui`, `soundhop service install`, or `soundhop setup`)");
        #[cfg(not(windows))]
        warning("Background Daemon: Not running (run `soundhop daemon` or `soundhop setup`)");
    }

    #[cfg(windows)]
    {
        let autostart_enabled = outcome
            .svc
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || outcome
                .svc
                .get("tray_autostart")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        if autostart_enabled {
            ok(r"Startup Autostart: Enabled (HKCU\Software\Microsoft\Windows\CurrentVersion\Run)");
        } else {
            info("Startup Autostart: Disabled (run `soundhop service install` to register HKCU Run autostart)");
        }
        info(&format!("Log File: {}", outcome.daemon_log_path.display()));
    }

    #[cfg(not(windows))]
    {
        let _ = (&outcome.svc, &outcome.daemon_log_path);
    }

    let scrcpy_ready = outcome
        .tools
        .get("ready")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if scrcpy_ready {
        ok(&format!(
            "Android Bridge: scrcpy {} + adb {}",
            outcome
                .tools
                .get("scrcpy_version")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            outcome
                .tools
                .get("adb_version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ));
    } else {
        info("Android Bridge: Optional scrcpy/adb not installed (`soundhop android install`)");
    }

    if outcome.appimage_mounts.len() > 1 {
        warning(&format!(
            "AppImage Mounts: {} active mounts detected (run `soundhop doctor --fix` to clean up stale mounts)",
            outcome.appimage_mounts.len()
        ));
    }

    for act in &outcome.fixed_actions {
        ok(&format!("Fixed: {act}"));
    }
    for ferr in &outcome.fix_errors {
        error(ferr);
    }

    if outcome.exit_ok {
        0
    } else {
        1
    }
}

pub fn run_bug_report(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let bundle = collect_bug_report(controller, &SystemDoctorEnv);

    if json_out {
        print_json(&bundle);
    } else {
        header("SoundHop Diagnostic Bug Report");
        safe_print(&serde_json::to_string_pretty(&bundle).unwrap_or_default());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

    struct MockDoctorEnv {
        firewall: Mutex<FirewallState>,
        open_fw_ok: bool,
        open_fw_errors: Vec<String>,
        open_fw_calls: Mutex<Vec<FirewallPorts>>,
        service_active: Mutex<bool>,
        install_svc_started: bool,
        install_svc_calls: Mutex<usize>,
        ipc_running: Mutex<bool>,
        addresses: Vec<String>,
        mounts: Mutex<Vec<String>>,
        own_mount: Option<String>,
        unmount_fail_target: Option<String>,
        unmounted: Mutex<Vec<String>>,
    }

    #[allow(clippy::field_reassign_with_default)]
    fn mock_fw(kind: &str, active: bool, detail: &str) -> FirewallState {
        let mut fw = FirewallState::default();
        fw.kind = kind.to_string();
        fw.active = active;
        fw.detail = detail.to_string();
        fw
    }

    impl Default for MockDoctorEnv {
        fn default() -> Self {
            Self {
                firewall: Mutex::new(mock_fw("ufw", false, "inactive")),
                open_fw_ok: true,
                open_fw_errors: Vec::new(),
                open_fw_calls: Mutex::new(Vec::new()),
                service_active: Mutex::new(true),
                install_svc_started: true,
                install_svc_calls: Mutex::new(0),
                ipc_running: Mutex::new(true),
                addresses: vec!["192.168.1.42".to_string()],
                mounts: Mutex::new(Vec::new()),
                own_mount: None,
                unmount_fail_target: None,
                unmounted: Mutex::new(Vec::new()),
            }
        }
    }

    impl DoctorEnv for MockDoctorEnv {
        fn detect_firewall(&self, _ports: &FirewallPorts) -> FirewallState {
            self.firewall.lock().unwrap().clone()
        }

        fn open_firewall(&self, ports: &FirewallPorts) -> FirewallResult {
            self.open_fw_calls.lock().unwrap().push(ports.clone());
            let fw = self.firewall.lock().unwrap().clone();
            FirewallResult {
                firewall: fw,
                commands: vec![vec!["ufw".to_string(), "allow".to_string()]],
                ran: true,
                ok: self.open_fw_ok,
                errors: self.open_fw_errors.clone(),
                note: String::new(),
            }
        }

        fn service_status(&self) -> serde_json::Value {
            let active = *self.service_active.lock().unwrap();
            serde_json::json!({
                "installed": true,
                "enabled": active,
                "active": active,
            })
        }

        fn install_service(&self) -> serde_json::Value {
            *self.install_svc_calls.lock().unwrap() += 1;
            if self.install_svc_started {
                *self.service_active.lock().unwrap() = true;
            }
            serde_json::json!({
                "enabled": self.install_svc_started,
                "started": self.install_svc_started,
            })
        }

        fn ipc_running(&self) -> bool {
            *self.ipc_running.lock().unwrap()
        }

        fn local_addresses(&self) -> Vec<String> {
            self.addresses.clone()
        }

        fn list_appimage_mounts(&self) -> Vec<String> {
            self.mounts.lock().unwrap().clone()
        }

        fn own_appimage_mount(&self) -> Option<String> {
            self.own_mount.clone()
        }

        fn unmount_fuse(&self, mount_point: &str) -> Result<(), String> {
            if self.unmount_fail_target.as_deref() == Some(mount_point) {
                return Err(format!(
                    "Failed to unmount stale AppImage mount {mount_point}: busy"
                ));
            }
            self.unmounted.lock().unwrap().push(mount_point.to_string());
            self.mounts.lock().unwrap().retain(|m| m != mount_point);
            Ok(())
        }
    }

    fn make_test_controller(backend_available: bool) -> (tempfile::TempDir, Arc<Controller>) {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");

        let runner = Arc::new(MockRunner::new(move |args, _| {
            if !backend_available {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "Connection refused".to_string(),
                });
            }
            let joined = args.join(" ");
            let stdout = if joined.ends_with(" info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\n\
                 Server Version: 15.0.0\n\
                 Default Sink: alsa_output.mock\n\
                 Default Source: alsa_input.mock\n"
                    .to_string()
            } else {
                String::new()
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout,
                stderr: String::new(),
            })
        }));

        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let mut cfg = Config::default();
        cfg.sharing.enabled = false;
        cfg.sharing.port = 4713;
        cfg.discovery.port = 47777;
        cfg.save(Some(&cfg_path)).unwrap();

        let ctrl = Arc::new(Controller::new(
            Some(cfg),
            Some(State::default()),
            Some(backend),
            Some(cfg_path),
            Some(state_path),
            false,
        ));
        (dir, ctrl)
    }

    #[test]
    fn test_parse_appimage_mounts_deduplicates_and_filters() {
        let mountinfo = "\
22 1 0:21 / /proc rw,nosuid - proc proc rw
98 22 0:55 / /tmp/.mount_SoundHabc123 ro,nosuid - fuse.SoundHop.AppImage SoundHop ro
99 22 0:56 / /tmp/.mount_SoundHxyz789 ro,nosuid - fuse.SoundHop.AppImage SoundHop ro
100 22 0:55 / /tmp/.mount_SoundHabc123 ro,nosuid - fuse.SoundHop.AppImage SoundHop ro
101 22 0:57 / /tmp/.mount_OtherApp ro,nosuid - fuse.Other Other ro
";
        let mounts = parse_appimage_mounts(mountinfo);
        assert_eq!(
            mounts,
            vec![
                "/tmp/.mount_SoundHabc123".to_string(),
                "/tmp/.mount_SoundHxyz789".to_string(),
            ]
        );
    }

    #[test]
    fn test_doctor_healthy_json_report() {
        let (_dir, ctrl) = make_test_controller(true);
        let env = MockDoctorEnv::default();

        let outcome = collect_doctor_report(&ctrl, false, &env);
        assert!(outcome.exit_ok);
        assert_eq!(outcome.report["ok"], true);
        assert_eq!(outcome.report["healthy"], true);
        assert_eq!(outcome.report["version"], VERSION);
        assert_eq!(outcome.report["protocol"], PROTOCOL_VERSION);
        assert_eq!(outcome.report["backend"]["available"], true);
        assert_eq!(outcome.report["backend"]["pipewire"], true);
        assert_eq!(
            outcome.report["backend"]["default_sink"],
            "alsa_output.mock"
        );
        assert_eq!(outcome.report["network"]["addresses"][0], "192.168.1.42");
        assert_eq!(outcome.report["network"]["sharing_port"], 4713);
        assert_eq!(outcome.report["daemon_ipc_running"], true);
        assert_eq!(outcome.report["fixed_actions"].as_array().unwrap().len(), 0);
        assert_eq!(outcome.report["fix_errors"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_doctor_unhealthy_backend_and_missing_lan_addresses() {
        let (_dir, ctrl_down) = make_test_controller(false);
        let env = MockDoctorEnv::default();
        let down = collect_doctor_report(&ctrl_down, false, &env);
        assert!(!down.exit_ok);
        assert_eq!(down.report["ok"], false);
        assert_eq!(down.report["healthy"], false);
        assert_eq!(down.report["backend"]["available"], false);

        // Backend up, but no LAN IPv4 addresses -> ok=true, healthy=false
        let (_dir2, ctrl_up) = make_test_controller(true);
        let env_no_lan = MockDoctorEnv {
            addresses: Vec::new(),
            ..Default::default()
        };
        let no_lan = collect_doctor_report(&ctrl_up, false, &env_no_lan);
        assert!(no_lan.exit_ok);
        assert_eq!(no_lan.report["ok"], true);
        assert_eq!(no_lan.report["healthy"], false);
    }

    #[test]
    fn test_doctor_fix_opens_firewall_starts_service_and_cleans_stale_mounts() {
        let (_dir, ctrl) = make_test_controller(true);
        let env = MockDoctorEnv {
            firewall: Mutex::new(mock_fw("ufw", true, "active")),
            open_fw_ok: true,
            service_active: Mutex::new(false),
            install_svc_started: true,
            ipc_running: Mutex::new(false),
            mounts: Mutex::new(vec![
                "/tmp/.mount_SoundH_current".to_string(),
                "/tmp/.mount_SoundH_stale1".to_string(),
            ]),
            own_mount: Some("/tmp/.mount_SoundH_current".to_string()),
            ..Default::default()
        };

        let outcome = collect_doctor_report(&ctrl, true, &env);
        assert!(outcome.exit_ok);
        assert_eq!(outcome.report["ok"], true);
        assert_eq!(outcome.report["healthy"], true);
        assert!(outcome.fix_errors.is_empty());

        // Firewall fix ran with configured ports
        assert_eq!(
            env.open_fw_calls.lock().unwrap().as_slice(),
            &[FirewallPorts {
                legacy_audio_tcp: Some(4713),
                control_tcp: 4714,
                pair_tcp: 4715,
                rtp_udp: 4716,
                discovery_udp: 47777,
                include_mdns: true,
            }]
        );
        assert!(outcome
            .fixed_actions
            .iter()
            .any(|a| a.contains("Opened firewall ports on ufw")));

        // Service install ran once
        assert_eq!(*env.install_svc_calls.lock().unwrap(), 1);
        assert!(outcome.svc_active);

        // Only the stale mount was unmounted, leaving APPDIR untouched
        assert_eq!(
            env.unmounted.lock().unwrap().as_slice(),
            &["/tmp/.mount_SoundH_stale1".to_string()]
        );
        assert_eq!(
            outcome.appimage_mounts,
            vec!["/tmp/.mount_SoundH_current".to_string()]
        );
    }

    #[test]
    fn test_doctor_fix_records_errors_and_fails_exit_status() {
        let (_dir, ctrl) = make_test_controller(true);
        let env = MockDoctorEnv {
            firewall: Mutex::new(mock_fw("firewalld", true, "running")),
            open_fw_ok: false,
            open_fw_errors: vec!["permission denied".to_string()],
            service_active: Mutex::new(false),
            install_svc_started: false,
            ipc_running: Mutex::new(false),
            mounts: Mutex::new(vec![
                "/tmp/.mount_SoundH_a".to_string(),
                "/tmp/.mount_SoundH_b".to_string(),
            ]),
            own_mount: Some("/tmp/.mount_SoundH_a".to_string()),
            unmount_fail_target: Some("/tmp/.mount_SoundH_b".to_string()),
            ..Default::default()
        };

        let outcome = collect_doctor_report(&ctrl, true, &env);
        assert!(!outcome.exit_ok);
        assert_eq!(outcome.report["ok"], false);
        assert_eq!(outcome.report["healthy"], false);
        assert!(outcome
            .fix_errors
            .iter()
            .any(|e| e.contains("Firewall (firewalld): permission denied")));
        assert!(outcome
            .fix_errors
            .iter()
            .any(|e| e.contains("Failed to install/start")));
        assert!(outcome
            .fix_errors
            .iter()
            .any(|e| e.contains("Failed to unmount stale AppImage mount /tmp/.mount_SoundH_b")));
    }

    #[test]
    fn test_collect_bug_report_contains_required_json_fields() {
        let (_dir, ctrl) = make_test_controller(true);
        let env = MockDoctorEnv::default();

        let bundle = collect_bug_report(&ctrl, &env);
        assert_eq!(bundle["version"], VERSION);
        assert_eq!(bundle["protocol"], PROTOCOL_VERSION);
        assert_eq!(bundle["os"], std::env::consts::OS);
        assert_eq!(bundle["arch"], std::env::consts::ARCH);
        assert!(bundle["config_path"].is_string());
        assert!(bundle["state_path"].is_string());
        assert!(bundle["log_path"].is_string());
        assert!(bundle["status"].is_object());
        assert_eq!(bundle["firewall"]["kind"], "ufw");
        assert_eq!(bundle["firewall"]["active"], false);
        assert_eq!(bundle["service"]["active"], true);
    }
}
