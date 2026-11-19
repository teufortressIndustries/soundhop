//! Firewall inspection and rule helpers for UFW, firewalld, nftables, and Windows Defender.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use crate::constants::MDNS_PORT;
use crate::util::{command_exists, ProcessRunner, SystemRunner};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallState {
    pub kind: String,
    pub active: bool,
    pub detail: String,
}

impl Default for FirewallState {
    fn default() -> Self {
        Self {
            kind: "none".to_string(),
            active: false,
            detail: "No supported firewall detected".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallResult {
    pub firewall: FirewallState,
    pub commands: Vec<Vec<String>>,
    pub ran: bool,
    pub ok: bool,
    pub errors: Vec<String>,
    pub note: String,
}

impl FirewallResult {
    pub fn to_json(&self) -> serde_json::Value {
        let cmd_strings: Vec<String> = self.commands.iter().map(|c| c.join(" ")).collect();
        serde_json::json!({
            "firewall": self.firewall,
            "commands": cmd_strings,
            "ran": self.ran,
            "ok": self.ok,
            "errors": self.errors,
            "note": self.note,
        })
    }
}

pub fn is_admin() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn parse_ufw_conf_enabled(content: &str) -> Option<bool> {
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("ENABLED") {
            let cleaned = val.trim().trim_matches(|c| c == '"' || c == '\'');
            if cleaned.eq_ignore_ascii_case("yes") {
                return Some(true);
            }
            if cleaned.eq_ignore_ascii_case("no") {
                return Some(false);
            }
        }
    }
    None
}

pub fn detect() -> FirewallState {
    detect_with_runner(&SystemRunner)
}

pub fn detect_with_runner(runner: &dyn ProcessRunner) -> FirewallState {
    detect_with_runner_and_ufw_conf(runner, Path::new("/etc/ufw/ufw.conf"))
}

fn detect_with_runner_and_ufw_conf(
    runner: &dyn ProcessRunner,
    ufw_conf_path: &Path,
) -> FirewallState {
    if cfg!(windows) {
        if runner.command_exists("netsh") {
            let args = vec![
                "netsh".to_string(),
                "advfirewall".to_string(),
                "show".to_string(),
                "allprofiles".to_string(),
                "state".to_string(),
            ];
            if let Ok(res) = runner.run(&args, None, Duration::from_secs(5)) {
                if res.ok() {
                    let lower = res.stdout.to_lowercase();
                    let active = lower.contains("state") && lower.contains("on");
                    return FirewallState {
                        kind: "windows-defender".to_string(),
                        active,
                        detail: if active {
                            "Windows Defender Firewall is active".to_string()
                        } else {
                            "Windows Defender Firewall is off".to_string()
                        },
                    };
                }
            }
        }
        return FirewallState {
            kind: "none".to_string(),
            active: false,
            detail: "Windows Defender Firewall state could not be determined".to_string(),
        };
    }

    if runner.command_exists("ufw") {
        let args = vec!["ufw".to_string(), "status".to_string()];
        if let Ok(res) = runner.run(&args, None, Duration::from_secs(5)) {
            let output = format!("{}{}", res.stdout, res.stderr).to_lowercase();
            if output.contains("status: active") {
                return FirewallState {
                    kind: "ufw".to_string(),
                    active: true,
                    detail: "ufw is active".to_string(),
                };
            }
            if output.contains("status: inactive") {
                return FirewallState {
                    kind: "ufw".to_string(),
                    active: false,
                    detail: "ufw is installed but inactive".to_string(),
                };
            }
        }
        // `ufw status` requires root on most Linux distributions. Fall back to
        // reading `/etc/ufw/ufw.conf` (world-readable) or querying systemd.
        if let Ok(conf) = fs::read_to_string(ufw_conf_path) {
            if let Some(enabled) = parse_ufw_conf_enabled(&conf) {
                return FirewallState {
                    kind: "ufw".to_string(),
                    active: enabled,
                    detail: if enabled {
                        "ufw is active (detected from ufw.conf)".to_string()
                    } else {
                        "ufw is installed but inactive (detected from ufw.conf)".to_string()
                    },
                };
            }
        }
        if let Ok(res) = runner.run(
            &[
                "systemctl".to_string(),
                "is-active".to_string(),
                "--quiet".to_string(),
                "ufw".to_string(),
            ],
            None,
            Duration::from_secs(5),
        ) {
            if res.ok() {
                return FirewallState {
                    kind: "ufw".to_string(),
                    active: true,
                    detail: "ufw is active (detected via systemd)".to_string(),
                };
            }
        }
        return FirewallState {
            kind: "ufw".to_string(),
            active: false,
            detail: "ufw state could not be determined".to_string(),
        };
    }

    if runner.command_exists("firewall-cmd") {
        let args = vec![
            "systemctl".to_string(),
            "is-active".to_string(),
            "--quiet".to_string(),
            "firewalld".to_string(),
        ];
        if let Ok(res) = runner.run(&args, None, Duration::from_secs(5)) {
            if res.ok() {
                return FirewallState {
                    kind: "firewalld".to_string(),
                    active: true,
                    detail: "firewalld is active".to_string(),
                };
            }
            return FirewallState {
                kind: "firewalld".to_string(),
                active: false,
                detail: "firewalld is installed but inactive".to_string(),
            };
        }
    }

    if runner.command_exists("nft") {
        let mut active = false;
        let mut detail =
            "nftables is installed but inactive or status could not be determined".to_string();
        if let Ok(res) = runner.run(
            &[
                "systemctl".to_string(),
                "is-active".to_string(),
                "--quiet".to_string(),
                "nftables".to_string(),
            ],
            None,
            Duration::from_secs(5),
        ) {
            if res.ok() {
                active = true;
                detail =
                    "nftables is active; SoundHop will print commands instead of editing rules"
                        .to_string();
            }
        } else if is_admin() {
            if let Ok(res) = runner.run(
                &["nft".to_string(), "list".to_string(), "tables".to_string()],
                None,
                Duration::from_secs(5),
            ) {
                if res.ok() && !res.stdout.trim().is_empty() {
                    active = true;
                    detail =
                        "nftables is active; SoundHop will print commands instead of editing rules"
                            .to_string();
                }
            }
        }
        return FirewallState {
            kind: "nftables".to_string(),
            active,
            detail,
        };
    }

    FirewallState::default()
}

fn sudo_prefix(interactive: bool) -> Vec<String> {
    if is_admin() || !command_exists("sudo") {
        return Vec::new();
    }
    if interactive && std::io::stdin().is_terminal() {
        vec!["sudo".to_string()]
    } else {
        vec!["sudo".to_string(), "-n".to_string()]
    }
}

fn execute_firewall_commands(result: &mut FirewallResult, interactive: bool) {
    result.ran = true;
    for cmd in &result.commands {
        let is_interactive_sudo = interactive
            && cmd.first().map(|s| s.as_str()) == Some("sudo")
            && !cmd.iter().any(|a| a == "-n");
        let run_res = if is_interactive_sudo {
            SystemRunner.run_interactive(cmd, None, Duration::from_secs(60))
        } else {
            SystemRunner.run(cmd, None, Duration::from_secs(15))
        };
        match run_res {
            Ok(res) if res.ok() => {}
            Ok(res) => {
                result.ok = false;
                result.errors.push(res.combined_output());
            }
            Err(err) => {
                result.ok = false;
                result.errors.push(err.to_string());
            }
        }
    }
}

pub fn open_ports(
    audio_port: u16,
    discovery_port: u16,
    dry_run: bool,
    include_mdns: bool,
) -> FirewallResult {
    open_ports_with_mode(audio_port, discovery_port, dry_run, include_mdns, true)
}

/// Non-interactive variant for background daemons/IPC: always uses `sudo -n` so it
/// never blocks on a TTY password prompt.
pub fn open_ports_non_interactive(
    audio_port: u16,
    discovery_port: u16,
    dry_run: bool,
    include_mdns: bool,
) -> FirewallResult {
    open_ports_with_mode(audio_port, discovery_port, dry_run, include_mdns, false)
}

fn open_ports_with_mode(
    audio_port: u16,
    discovery_port: u16,
    dry_run: bool,
    include_mdns: bool,
    interactive: bool,
) -> FirewallResult {
    let state = detect();
    let mut result = FirewallResult {
        firewall: state.clone(),
        commands: Vec::new(),
        ran: false,
        ok: true,
        errors: Vec::new(),
        note: String::new(),
    };
    if !state.active {
        result.note = state.detail;
        return result;
    }

    let sudo = sudo_prefix(interactive);
    match state.kind.as_str() {
        "ufw" => {
            let mut c1 = sudo.clone();
            c1.extend(["ufw".into(), "allow".into(), format!("{audio_port}/tcp")]);
            result.commands.push(c1);

            let mut c2 = sudo.clone();
            c2.extend([
                "ufw".into(),
                "allow".into(),
                format!("{discovery_port}/udp"),
            ]);
            result.commands.push(c2);

            if include_mdns {
                let mut c3 = sudo;
                c3.extend(["ufw".into(), "allow".into(), format!("{MDNS_PORT}/udp")]);
                result.commands.push(c3);
            }
        }
        "firewalld" => {
            let mut c1 = sudo.clone();
            c1.extend([
                "firewall-cmd".into(),
                format!("--add-port={audio_port}/tcp"),
                "--permanent".into(),
            ]);
            result.commands.push(c1);

            let mut c2 = sudo.clone();
            c2.extend([
                "firewall-cmd".into(),
                format!("--add-port={discovery_port}/udp"),
                "--permanent".into(),
            ]);
            result.commands.push(c2);

            if include_mdns {
                let mut c3 = sudo.clone();
                c3.extend([
                    "firewall-cmd".into(),
                    format!("--add-port={MDNS_PORT}/udp"),
                    "--permanent".into(),
                ]);
                result.commands.push(c3);

                let mut c4 = sudo.clone();
                c4.extend([
                    "firewall-cmd".into(),
                    "--add-service=mdns".into(),
                    "--permanent".into(),
                ]);
                result.commands.push(c4);
            }
            let mut reload = sudo;
            reload.extend(["firewall-cmd".into(), "--reload".into()]);
            result.commands.push(reload);
        }
        "windows-defender" => {
            result.commands.push(vec![
                "netsh".into(),
                "advfirewall".into(),
                "firewall".into(),
                "add".into(),
                "rule".into(),
                format!("name=SoundHop Audio ({audio_port}/tcp)"),
                "dir=in".into(),
                "action=allow".into(),
                "protocol=TCP".into(),
                format!("localport={audio_port}"),
            ]);
            result.commands.push(vec![
                "netsh".into(),
                "advfirewall".into(),
                "firewall".into(),
                "add".into(),
                "rule".into(),
                format!("name=SoundHop Discovery ({discovery_port}/udp)"),
                "dir=in".into(),
                "action=allow".into(),
                "protocol=UDP".into(),
                format!("localport={discovery_port}"),
            ]);
            if include_mdns {
                result.commands.push(vec![
                    "netsh".into(),
                    "advfirewall".into(),
                    "firewall".into(),
                    "add".into(),
                    "rule".into(),
                    format!("name=SoundHop mDNS ({MDNS_PORT}/udp)"),
                    "dir=in".into(),
                    "action=allow".into(),
                    "protocol=UDP".into(),
                    format!("localport={MDNS_PORT}"),
                ]);
            }
        }
        _ => {
            result.note = format!(
                "nftables detected. Add these rules yourself:\n  nft add rule inet filter input tcp dport {audio_port} accept\n  nft add rule inet filter input udp dport {discovery_port} accept"
            );
            return result;
        }
    }

    if dry_run {
        return result;
    }

    execute_firewall_commands(&mut result, interactive);
    result
}

pub fn close_ports(audio_port: u16, discovery_port: u16, dry_run: bool) -> FirewallResult {
    let state = detect();
    let mut result = FirewallResult {
        firewall: state.clone(),
        commands: Vec::new(),
        ran: false,
        ok: true,
        errors: Vec::new(),
        note: String::new(),
    };
    if !state.active {
        result.note = state.detail;
        return result;
    }
    let sudo = sudo_prefix(true);
    match state.kind.as_str() {
        "ufw" => {
            let mut c1 = sudo.clone();
            c1.extend([
                "ufw".into(),
                "delete".into(),
                "allow".into(),
                format!("{audio_port}/tcp"),
            ]);
            result.commands.push(c1);

            let mut c2 = sudo;
            c2.extend([
                "ufw".into(),
                "delete".into(),
                "allow".into(),
                format!("{discovery_port}/udp"),
            ]);
            result.commands.push(c2);
        }
        "firewalld" => {
            let mut c1 = sudo.clone();
            c1.extend([
                "firewall-cmd".into(),
                format!("--remove-port={audio_port}/tcp"),
                "--permanent".into(),
            ]);
            result.commands.push(c1);

            let mut c2 = sudo.clone();
            c2.extend([
                "firewall-cmd".into(),
                format!("--remove-port={discovery_port}/udp"),
                "--permanent".into(),
            ]);
            result.commands.push(c2);

            let mut reload = sudo;
            reload.extend(["firewall-cmd".into(), "--reload".into()]);
            result.commands.push(reload);
        }
        "windows-defender" => {
            result.commands.push(vec![
                "netsh".into(),
                "advfirewall".into(),
                "firewall".into(),
                "delete".into(),
                "rule".into(),
                format!("name=SoundHop Audio ({audio_port}/tcp)"),
            ]);
            result.commands.push(vec![
                "netsh".into(),
                "advfirewall".into(),
                "firewall".into(),
                "delete".into(),
                "rule".into(),
                format!("name=SoundHop Discovery ({discovery_port}/udp)"),
            ]);
        }
        _ => {
            result.note =
                "nftables is active; remove SoundHop rules manually with `nft`.".to_string();
            return result;
        }
    }

    if dry_run {
        return result;
    }
    execute_firewall_commands(&mut result, true);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{CommandResult, MockRunner};

    #[test]
    #[cfg(not(windows))]
    fn test_detect_ufw() {
        let runner = MockRunner::new(|args, _| {
            let stdout = if args.first().map(|s| s.as_str()) == Some("ufw") {
                "Status: active\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        });
        runner
            .available_commands
            .lock()
            .unwrap()
            .insert("ufw".to_string());
        let st = detect_with_runner(&runner);
        assert_eq!(st.kind, "ufw");
        assert!(st.active);
    }

    #[test]
    #[cfg(not(windows))]
    fn test_detect_ufw_non_root_falls_back_to_ufw_conf_and_systemd() {
        let dir = tempfile::tempdir().unwrap();
        let conf_yes = dir.path().join("ufw_yes.conf");
        fs::write(&conf_yes, "# comment\nENABLED=yes\nLOGLEVEL=low\n").unwrap();
        let conf_no = dir.path().join("ufw_no.conf");
        fs::write(&conf_no, "ENABLED=no\n").unwrap();
        let conf_missing = dir.path().join("missing.conf");

        let runner = MockRunner::new(|args, _| {
            if args.first().map(|s| s.as_str()) == Some("ufw") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "ERROR: You need to be root to run this script\n".to_string(),
                });
            }
            if args.first().map(|s| s.as_str()) == Some("systemctl") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 1,
                stdout: String::new(),
                stderr: String::new(),
            })
        });
        runner
            .available_commands
            .lock()
            .unwrap()
            .insert("ufw".to_string());

        let st_yes = detect_with_runner_and_ufw_conf(&runner, &conf_yes);
        assert_eq!(st_yes.kind, "ufw");
        assert!(st_yes.active);

        let st_no = detect_with_runner_and_ufw_conf(&runner, &conf_no);
        assert_eq!(st_no.kind, "ufw");
        assert!(!st_no.active);

        let st_systemd = detect_with_runner_and_ufw_conf(&runner, &conf_missing);
        assert_eq!(st_systemd.kind, "ufw");
        assert!(st_systemd.active);
    }

    #[test]
    #[cfg(not(windows))]
    fn test_detect_nftables_inactive_when_systemctl_fails() {
        let runner = MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 3,
                stdout: String::new(),
                stderr: "inactive".to_string(),
            })
        });
        runner
            .available_commands
            .lock()
            .unwrap()
            .insert("nft".to_string());
        let st = detect_with_runner(&runner);
        assert_eq!(st.kind, "nftables");
        assert!(!st.active);
    }

    #[test]
    #[cfg(windows)]
    fn test_detect_windows_defender() {
        let runner = MockRunner::new(|args, _| {
            let stdout = if args.first().map(|s| s.as_str()) == Some("netsh") {
                "State ON\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        });
        runner
            .available_commands
            .lock()
            .unwrap()
            .insert("netsh".to_string());
        let st = detect_with_runner(&runner);
        assert_eq!(st.kind, "windows-defender");
        assert!(st.active);
    }
}
