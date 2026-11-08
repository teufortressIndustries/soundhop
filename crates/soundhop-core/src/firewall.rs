//! Firewall inspection and rule helpers for UFW, firewalld, nftables, and Windows Defender.

use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
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

pub fn detect() -> FirewallState {
    detect_with_runner(&SystemRunner)
}

pub fn detect_with_runner(runner: &dyn ProcessRunner) -> FirewallState {
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
            return FirewallState {
                kind: "ufw".to_string(),
                active: false,
                detail: "ufw state could not be determined".to_string(),
            };
        }
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
        return FirewallState {
            kind: "nftables".to_string(),
            active: true,
            detail: "nftables is present; SoundHop will print commands instead of editing rules"
                .to_string(),
        };
    }

    FirewallState::default()
}

fn sudo_prefix() -> Vec<String> {
    if is_admin() || !command_exists("sudo") {
        return Vec::new();
    }
    if std::io::stdin().is_terminal() {
        vec!["sudo".to_string()]
    } else {
        vec!["sudo".to_string(), "-n".to_string()]
    }
}

pub fn open_ports(
    audio_port: u16,
    discovery_port: u16,
    dry_run: bool,
    include_mdns: bool,
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

    let sudo = sudo_prefix();
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

    result.ran = true;
    for cmd in &result.commands {
        match SystemRunner.run(cmd, None, Duration::from_secs(30)) {
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
    let sudo = sudo_prefix();
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
        _ => return result,
    }

    if dry_run {
        return result;
    }
    result.ran = true;
    for cmd in &result.commands {
        if let Ok(res) = SystemRunner.run(cmd, None, Duration::from_secs(30)) {
            if !res.ok() {
                result.ok = false;
                result.errors.push(res.combined_output());
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{CommandResult, MockRunner};

    #[test]
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
}
