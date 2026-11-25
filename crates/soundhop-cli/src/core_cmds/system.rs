//! System, setup, daemon, service, firewall, completions, and version CLI subcommand handlers.

use std::io;
use std::sync::Arc;

use clap::CommandFactory;
use clap_complete::{generate, shells};
use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::Controller;
use soundhop_core::daemon::Daemon;
use soundhop_core::firewall::{detect_for_config, open_ports_for_config};
use soundhop_core::service::{
    install as install_service, restart_service, start_service, status as service_status,
    stop_service, sync_appimage_paths_if_installed, uninstall as uninstall_service,
};

use crate::parser::{Cli, CompletionsArgs, FirewallArgs, ServiceArgs, SetupArgs, ShellChoice};
use crate::ui::{emit_error, header, info, lock_or_recover, ok, print_json, safe_print, warning};

pub fn handle_version(json_out: bool) -> i32 {
    if json_out {
        print_json(&serde_json::json!({
            "version": VERSION,
            "protocol": PROTOCOL_VERSION,
        }));
    } else {
        safe_print(&format!(
            "SoundHop v{VERSION} (protocol v{PROTOCOL_VERSION})"
        ));
    }
    0
}

pub fn handle_completions(args: &CompletionsArgs) -> i32 {
    let mut cmd = Cli::command();
    let mut stdout = io::stdout();
    match args.shell {
        ShellChoice::Bash => generate(shells::Bash, &mut cmd, "soundhop", &mut stdout),
        ShellChoice::Zsh => generate(shells::Zsh, &mut cmd, "soundhop", &mut stdout),
        ShellChoice::Fish => generate(shells::Fish, &mut cmd, "soundhop", &mut stdout),
        ShellChoice::Powershell => generate(shells::PowerShell, &mut cmd, "soundhop", &mut stdout),
    }
    0
}

pub fn handle_setup(controller: &Arc<Controller>, args: &SetupArgs, json_out: bool) -> i32 {
    if let Some(ref name) = args.name {
        if !name.trim().is_empty() {
            lock_or_recover(&controller.config).device.name = name.trim().to_string();
        }
    }
    lock_or_recover(&controller.config).sharing.enabled = true;
    controller.save();

    let share_res = controller.share(true, Some(!json_out));
    let cfg = lock_or_recover(&controller.config).clone();

    let fw_res = if !args.skip_firewall {
        Some(open_ports_for_config(&cfg, false))
    } else {
        None
    };

    let svc_res = if !args.skip_service {
        Some(install_service(true, true, true))
    } else {
        None
    };

    let share_ok = share_res.is_ok();
    let fw_ok = fw_res.as_ref().is_none_or(|f| !f.ran || f.ok);
    let svc_ok = svc_res.as_ref().is_none_or(|s| {
        s.get("started").and_then(|v| v.as_bool()).unwrap_or(false)
            || s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)
    });
    let all_ok = share_ok && fw_ok && svc_ok;

    if json_out {
        let share_err = share_res.as_ref().err().map(|e| e.to_string());
        print_json(&serde_json::json!({
            "ok": all_ok,
            "non_interactive": args.yes,
            "device": cfg.device.name,
            "sharing": share_res.ok(),
            "sharing_error": share_err,
            "firewall": fw_res.map(|f| f.to_json()),
            "service": svc_res,
        }));
        return if all_ok { 0 } else { 1 };
    }

    header(&format!(
        "SoundHop v{VERSION} Setup {}",
        if all_ok { "Complete" } else { "Incomplete" }
    ));
    ok(&format!("Device Name: {}", cfg.device.name));
    match share_res {
        Ok(_) => ok(&format!(
            "Speaker Sharing: Enabled on TCP port {}",
            cfg.sharing.port
        )),
        Err(e) => emit_error(&format!("Speaker Sharing: {e}"), false),
    }
    if let Some(fw) = fw_res {
        if fw.ok {
            ok(&format!("Firewall ({}): Ready", fw.firewall.kind));
        } else {
            warning(&format!(
                "Firewall ({}): {}",
                fw.firewall.kind,
                fw.errors.join("; ")
            ));
        }
    }
    if let Some(ref svc) = svc_res {
        let started = svc
            .get("started")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let enabled = svc
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if started {
            #[cfg(windows)]
            ok("Windows Startup (HKCU Run) & Background Tray: Installed and started");
            #[cfg(not(windows))]
            ok("Background Daemon & Tray Autostart: Installed and started");
        } else if enabled {
            #[cfg(windows)]
            warning(
                "Windows Startup (HKCU Run) & Background Tray: Enabled, but background tray failed to start",
            );
            #[cfg(not(windows))]
            warning(
                "Background Daemon & Tray Autostart: Installed and enabled, but failed to start",
            );
        } else {
            #[cfg(windows)]
            emit_error(
                "Windows Startup (HKCU Run) & Background Tray: Installation failed",
                false,
            );
            #[cfg(not(windows))]
            emit_error(
                "Background Daemon & Tray Autostart: Installation failed",
                false,
            );
        }
    }
    if all_ok {
        0
    } else {
        1
    }
}

pub fn handle_daemon(verbose: bool, json_out: bool) -> i32 {
    let daemon = Daemon::new(None, None, !verbose, None);
    match daemon.run_until_stopped() {
        Ok(()) => 0,
        Err(e) => {
            emit_error(&e, json_out);
            if e.contains("already running") {
                2
            } else {
                1
            }
        }
    }
}

pub fn handle_stop(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let quit_res = soundhop_core::service::quit_all_soundhop(None);
    let stopped_daemon = quit_res
        .get("daemon_stopped")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let _ = controller.disconnect(false, None, true, Some(false), true, true);
    if json_out {
        print_json(&serde_json::json!({
            "stopped_daemon": stopped_daemon,
            "disconnected": true,
            "service_stopped": quit_res.get("service_stopped").and_then(|v| v.as_bool()).unwrap_or(false),
            "terminated_pids": quit_res.get("terminated_pids").cloned().unwrap_or_else(|| serde_json::json!([])),
        }));
    } else {
        ok("Stopped SoundHop daemon and disconnected active audio sessions.");
    }
    0
}

pub fn handle_service(args: &ServiceArgs, json_out: bool) -> i32 {
    let act = args.action.trim().to_lowercase();
    let (res, action_ok) = match act.as_str() {
        "install" => {
            let v = install_service(true, true, !args.no_tray);
            let ok_flag = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false)
                && v.get("started").and_then(|x| x.as_bool()).unwrap_or(false);
            (v, ok_flag)
        }
        "uninstall" | "remove" => {
            let v = uninstall_service();
            let ok_flag = v.get("removed").and_then(|x| x.as_bool()).unwrap_or(false)
                || v.get("tray_removed")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
            (v, ok_flag)
        }
        "start" => {
            let ok_flag = start_service();
            (
                serde_json::json!({ "ok": ok_flag, "started": ok_flag }),
                ok_flag,
            )
        }
        "stop" => {
            let ok_flag = stop_service();
            (
                serde_json::json!({ "ok": ok_flag, "stopped": ok_flag }),
                ok_flag,
            )
        }
        "restart" => {
            let _ = sync_appimage_paths_if_installed(false);
            let ok_flag = restart_service();
            (
                serde_json::json!({ "ok": ok_flag, "restarted": ok_flag }),
                ok_flag,
            )
        }
        "status" => (service_status(), true),
        other => {
            emit_error(
                &format!(
                    "Unknown service action '{other}'. Expected: install, uninstall, start, stop, restart, status."
                ),
                json_out,
            );
            return 1;
        }
    };
    if json_out {
        print_json(&res);
    } else {
        match act.as_str() {
            "install" => {
                if action_ok {
                    #[cfg(windows)]
                    ok("Installed Windows HKCU Run startup entry and started SoundHop background tray.");
                    #[cfg(not(windows))]
                    ok("Installed, enabled, and started SoundHop user service.");
                    let entry_path = res
                        .get("registry_key")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            res.get("tray_autostart")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                        });
                    if let Some(tray_path) = entry_path {
                        #[cfg(windows)]
                        info(&format!("Startup entry: {tray_path}"));
                        #[cfg(not(windows))]
                        info(&format!("Tray autostart: {tray_path}"));
                    } else {
                        #[cfg(windows)]
                        info(
                            r"Startup entry: HKCU\Software\Microsoft\Windows\CurrentVersion\Run\SoundHop",
                        );
                    }
                } else if let Some(err) = res.get("error").and_then(|v| v.as_str()) {
                    emit_error(err, false);
                } else {
                    let enabled = res
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let started = res
                        .get("started")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    #[cfg(windows)]
                    emit_error(
                        &format!(
                            "Failed to install/start SoundHop Windows startup entry (enabled={enabled}, started={started})."
                        ),
                        false,
                    );
                    #[cfg(not(windows))]
                    emit_error(
                        &format!(
                            "Failed to install/start SoundHop user service (enabled={enabled}, started={started})."
                        ),
                        false,
                    );
                }
            }
            "uninstall" | "remove" => {
                if action_ok {
                    #[cfg(windows)]
                    ok("Removed SoundHop Windows HKCU Run startup entry and stopped background tray.");
                    #[cfg(not(windows))]
                    ok("Removed SoundHop user service and autostart entries.");
                } else {
                    #[cfg(windows)]
                    info("SoundHop Windows startup entry was not installed.");
                    #[cfg(not(windows))]
                    info("SoundHop user service was not installed.");
                }
            }
            "start" => {
                if action_ok {
                    #[cfg(windows)]
                    ok("Started SoundHop background tray service.");
                    #[cfg(not(windows))]
                    ok("Started SoundHop user service.");
                } else {
                    #[cfg(windows)]
                    emit_error("Failed to start SoundHop background tray service.", false);
                    #[cfg(not(windows))]
                    emit_error("Failed to start SoundHop user service.", false);
                }
            }
            "stop" => {
                if action_ok {
                    #[cfg(windows)]
                    ok("Stopped SoundHop background tray service.");
                    #[cfg(not(windows))]
                    ok("Stopped SoundHop user service.");
                } else {
                    #[cfg(windows)]
                    emit_error("Failed to stop SoundHop background tray service.", false);
                    #[cfg(not(windows))]
                    emit_error("Failed to stop SoundHop user service.", false);
                }
            }
            "restart" => {
                if action_ok {
                    #[cfg(windows)]
                    ok("Restarted SoundHop background tray service.");
                    #[cfg(not(windows))]
                    ok("Restarted SoundHop user service.");
                } else {
                    #[cfg(windows)]
                    emit_error("Failed to restart SoundHop background tray service.", false);
                    #[cfg(not(windows))]
                    emit_error("Failed to restart SoundHop user service.", false);
                }
            }
            "status" => {
                let installed = res
                    .get("installed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let enabled = res
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let active = res.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
                let tray = res
                    .get("tray_autostart")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let unit = res
                    .get("registry_key")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| res.get("unit").and_then(|v| v.as_str()))
                    .unwrap_or("");
                #[cfg(windows)]
                {
                    header("SoundHop Windows Startup & Background Service Status");
                    if active {
                        ok("Background Tray/Daemon: active (running)");
                    } else if installed {
                        warning("Background Tray/Daemon: inactive (stopped)");
                    } else {
                        info("Background Tray/Daemon: not running (startup entry not installed)");
                    }
                    info(&format!(
                        "HKCU Run Autostart: {}  Tray autostart: {}",
                        if enabled { "enabled" } else { "disabled" },
                        if tray { "yes" } else { "no" }
                    ));
                    if !unit.is_empty() {
                        info(&format!("Startup Entry: {unit}"));
                    } else if installed || enabled {
                        info(
                            r"Startup Entry: HKCU\Software\Microsoft\Windows\CurrentVersion\Run\SoundHop",
                        );
                    }
                }
                #[cfg(not(windows))]
                {
                    header("SoundHop User Service Status");
                    if active {
                        ok("Service: active (running)");
                    } else if installed {
                        warning("Service: inactive (stopped)");
                    } else {
                        info("Service: not installed");
                    }
                    info(&format!(
                        "Enabled: {}  Tray autostart: {}",
                        if enabled { "yes" } else { "no" },
                        if tray { "yes" } else { "no" }
                    ));
                    if !unit.is_empty() {
                        info(&format!("Unit: {unit}"));
                    }
                }
            }
            _ => {}
        }
    }
    if action_ok {
        0
    } else {
        1
    }
}

pub fn handle_firewall(controller: &Arc<Controller>, args: &FirewallArgs, json_out: bool) -> i32 {
    let cfg = lock_or_recover(&controller.config).clone();
    let act = args.action.trim().to_lowercase();
    match act.as_str() {
        "open" => {
            let res = open_ports_for_config(&cfg, false);
            if json_out {
                print_json(&res.to_json());
            } else if res.ok {
                ok(&format!("Firewall ({}): ports opened.", res.firewall.kind));
            } else {
                emit_error(
                    &format!(
                        "Firewall ({}): {}",
                        res.firewall.kind,
                        res.errors.join("; ")
                    ),
                    false,
                );
            }
            if res.ok {
                0
            } else {
                1
            }
        }
        "status" => {
            let st = detect_for_config(&cfg);
            if json_out {
                print_json(&st);
            } else {
                info(&format!(
                    "Firewall: {} (active: {} — {})",
                    st.kind, st.active, st.detail
                ));
            }
            0
        }
        other => {
            emit_error(
                &format!("Unknown firewall action '{other}'. Expected: status, open."),
                json_out,
            );
            1
        }
    }
}
