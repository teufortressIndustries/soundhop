//! Firewall inspection and rule helpers for UFW, firewalld, nftables, and Windows Defender.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use crate::constants::{DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, MDNS_PORT};
use crate::util::{ProcessRunner, SystemRunner};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallState {
    pub kind: String,
    pub active: bool,
    pub detail: String,
    #[serde(default)]
    pub ports_allowed: Option<bool>,
}

impl Default for FirewallState {
    fn default() -> Self {
        Self {
            kind: "none".to_string(),
            active: false,
            detail: "No supported firewall detected".to_string(),
            ports_allowed: None,
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
    #[cfg(windows)]
    {
        use std::mem::size_of;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut return_len: u32 = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut return_len,
            );
            let _ = CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn parse_windows_firewall_state(stdout: &str) -> bool {
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
            if key.eq_ignore_ascii_case("state") && val.eq_ignore_ascii_case("on") {
                return true;
            }
        }
    }
    false
}

fn windows_audio_rule_name(audio_port: u16) -> String {
    format!("SoundHop Audio ({audio_port}/tcp)")
}

fn windows_discovery_rule_name(discovery_port: u16) -> String {
    format!("SoundHop Discovery ({discovery_port}/udp)")
}

fn windows_mdns_rule_name() -> String {
    format!("SoundHop mDNS ({MDNS_PORT}/udp)")
}

fn windows_delete_rule_cmd(name: &str) -> Vec<String> {
    vec![
        "netsh".into(),
        "advfirewall".into(),
        "firewall".into(),
        "delete".into(),
        "rule".into(),
        format!("name={name}"),
    ]
}

fn windows_show_rule_cmd(name: &str) -> Vec<String> {
    vec![
        "netsh".into(),
        "advfirewall".into(),
        "firewall".into(),
        "show".into(),
        "rule".into(),
        format!("name={name}"),
    ]
}

fn windows_add_rule_cmd(name: &str, protocol: &str, port: u16) -> Vec<String> {
    vec![
        "netsh".into(),
        "advfirewall".into(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={name}"),
        "dir=in".into(),
        "action=allow".into(),
        format!("protocol={protocol}"),
        format!("localport={port}"),
        "profile=private,domain".into(),
    ]
}

fn build_windows_open_commands(
    audio_port: u16,
    discovery_port: u16,
    include_mdns: bool,
) -> Vec<Vec<String>> {
    let audio_name = windows_audio_rule_name(audio_port);
    let discovery_name = windows_discovery_rule_name(discovery_port);
    let mut cmds = vec![
        windows_delete_rule_cmd(&audio_name),
        windows_add_rule_cmd(&audio_name, "TCP", audio_port),
        windows_delete_rule_cmd(&discovery_name),
        windows_add_rule_cmd(&discovery_name, "UDP", discovery_port),
    ];
    if include_mdns {
        let mdns_name = windows_mdns_rule_name();
        cmds.push(windows_delete_rule_cmd(&mdns_name));
        cmds.push(windows_add_rule_cmd(&mdns_name, "UDP", MDNS_PORT));
    }
    cmds
}

fn build_windows_close_commands(audio_port: u16, discovery_port: u16) -> Vec<Vec<String>> {
    vec![
        windows_delete_rule_cmd(&windows_audio_rule_name(audio_port)),
        windows_delete_rule_cmd(&windows_discovery_rule_name(discovery_port)),
        windows_delete_rule_cmd(&windows_mdns_rule_name()),
    ]
}

#[cfg_attr(not(windows), allow(dead_code))]
fn is_netsh_delete_rule(cmd: &[String]) -> bool {
    cmd.first()
        .map(|s| s.eq_ignore_ascii_case("netsh"))
        .unwrap_or(false)
        && cmd.iter().any(|a| a.eq_ignore_ascii_case("delete"))
        && cmd.iter().any(|a| a.eq_ignore_ascii_case("rule"))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn extract_netsh_rule_name(cmd: &[String]) -> Option<&str> {
    cmd.iter().find_map(|arg| {
        let (k, v) = arg.split_once('=')?;
        if k.eq_ignore_ascii_case("name") && !v.is_empty() {
            Some(v)
        } else {
            None
        }
    })
}

fn is_netsh_no_matching_rules(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("no rules match")
        || lower.contains("no rule matches")
        || lower.contains("not found")
}

#[cfg_attr(not(windows), allow(dead_code))]
fn needs_cmd_quoting(s: &str) -> bool {
    s.is_empty()
        || s.chars().any(|c| {
            c.is_whitespace() || matches!(c, '"' | '&' | '|' | '<' | '>' | '^' | '(' | ')')
        })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn format_netsh_arg_for_cmd(arg: &str) -> String {
    if let Some((key, val)) = arg.split_once('=') {
        if !key.is_empty() && !needs_cmd_quoting(key) && needs_cmd_quoting(val) {
            let escaped = val.replace('"', "\\\"");
            return format!("{key}=\"{escaped}\"");
        }
    }
    if needs_cmd_quoting(arg) {
        let escaped = arg.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        arg.to_string()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn format_command_for_cmd(cmd: &[String]) -> String {
    cmd.iter()
        .map(|a| format_netsh_arg_for_cmd(a))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg_attr(not(windows), allow(dead_code))]
fn build_windows_elevated_cmd_params(commands: &[Vec<String>], is_close: bool) -> String {
    if is_close {
        let mut steps = Vec::new();
        for cmd in commands {
            if let Some(name) = extract_netsh_rule_name(cmd) {
                let show_cmd = format_command_for_cmd(&windows_show_rule_cmd(name));
                let del_cmd = format_command_for_cmd(cmd);
                steps.push(format!(
                    "{show_cmd} >nul 2>&1 && ({del_cmd} >nul 2>&1 || exit /b 1)"
                ));
            } else {
                steps.push(format!("{} >nul 2>&1", format_command_for_cmd(cmd)));
            }
        }
        if steps.is_empty() {
            return "/C \"exit /b 0\"".to_string();
        }
        format!("/C \"{} & exit /b 0\"", steps.join(" & "))
    } else {
        let mut groups = Vec::new();
        let mut i = 0;
        while i < commands.len() {
            if is_netsh_delete_rule(&commands[i])
                && i + 1 < commands.len()
                && !is_netsh_delete_rule(&commands[i + 1])
            {
                let del_cmd = format_command_for_cmd(&commands[i]);
                let add_cmd = format_command_for_cmd(&commands[i + 1]);
                groups.push(format!("({del_cmd} >nul 2>&1 & {add_cmd})"));
                i += 2;
            } else if is_netsh_delete_rule(&commands[i]) {
                let del_cmd = format_command_for_cmd(&commands[i]);
                groups.push(format!("({del_cmd} >nul 2>&1 & ver >nul)"));
                i += 1;
            } else {
                groups.push(format!("({})", format_command_for_cmd(&commands[i])));
                i += 1;
            }
        }
        if groups.is_empty() {
            return "/C \"exit /b 0\"".to_string();
        }
        format!("/C \"{}\"", groups.join(" && "))
    }
}

#[cfg(windows)]
fn to_wide_null(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn run_elevated_windows_cmd(params: &str, timeout: Duration) -> Result<u32, String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_CANCELLED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let verb = to_wide_null("runas");
    let file = to_wide_null("cmd.exe");
    let parameters = to_wide_null(params);

    unsafe {
        let mut sei: SHELLEXECUTEINFOW = zeroed();
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = parameters.as_ptr();
        sei.nShow = SW_HIDE;

        if ShellExecuteExW(&mut sei) == 0 {
            let err = GetLastError();
            if err == ERROR_CANCELLED {
                return Err("UAC elevation was cancelled by the user".to_string());
            }
            return Err(format!(
                "Failed to launch elevated firewall helper (Win32 error {err})"
            ));
        }

        if sei.hProcess.is_null() {
            return Err("Elevated firewall helper did not return a process handle".to_string());
        }

        let wait_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let wait_res = WaitForSingleObject(sei.hProcess, wait_ms);
        let outcome = if wait_res == WAIT_OBJECT_0 {
            let mut exit_code: u32 = 1;
            if GetExitCodeProcess(sei.hProcess, &mut exit_code) != 0 {
                Ok(exit_code)
            } else {
                let err = GetLastError();
                Err(format!(
                    "Failed to read elevated firewall helper exit code (Win32 error {err})"
                ))
            }
        } else if wait_res == WAIT_TIMEOUT {
            Err(format!(
                "Elevated firewall helper timed out after {:.0}s",
                timeout.as_secs_f64()
            ))
        } else {
            let err = GetLastError();
            Err(format!(
                "Waiting for elevated firewall helper failed (Win32 error {err})"
            ))
        };

        let _ = CloseHandle(sei.hProcess);
        outcome
    }
}

#[cfg(windows)]
fn execute_windows_firewall_commands(
    result: &mut FirewallResult,
    interactive: bool,
    is_close: bool,
) {
    if result.commands.is_empty() {
        return;
    }

    if is_admin() {
        result.ran = true;
        // SystemRunner sets CREATE_NO_WINDOW (0x0800_0000) on Windows so netsh never flashes a console.
        for cmd in &result.commands {
            let is_del = is_netsh_delete_rule(cmd);
            match SystemRunner.run(cmd, None, Duration::from_secs(15)) {
                Ok(res) if res.ok() => {}
                Ok(res) => {
                    if is_del && !is_close {
                        // Pre-add cleanup delete may report "No rules match" on first run.
                        continue;
                    }
                    if is_del && is_close {
                        let out = res.combined_output();
                        if is_netsh_no_matching_rules(&out) {
                            continue;
                        }
                        if let Some(name) = extract_netsh_rule_name(cmd) {
                            let show = windows_show_rule_cmd(name);
                            if let Ok(show_res) =
                                SystemRunner.run(&show, None, Duration::from_secs(10))
                            {
                                if !show_res.ok() {
                                    // Rule does not exist (localized Windows output).
                                    continue;
                                }
                            }
                        }
                    }
                    result.ok = false;
                    result.errors.push(res.combined_output());
                }
                Err(err) => {
                    result.ok = false;
                    result.errors.push(err.to_string());
                }
            }
        }
        return;
    }

    if !interactive {
        result.ran = false;
        result.ok = false;
        result.errors.push(
            "Administrator privileges are required to modify Windows Defender Firewall in non-interactive mode."
                .to_string(),
        );
        result.note =
            "Run interactively to approve the Windows UAC prompt, or run as Administrator."
                .to_string();
        return;
    }

    result.ran = true;
    let params = build_windows_elevated_cmd_params(&result.commands, is_close);
    match run_elevated_windows_cmd(&params, Duration::from_secs(60)) {
        Ok(0) => {
            result.ok = true;
            result.note = "Windows Defender Firewall rules updated via UAC elevation.".to_string();
        }
        Ok(code) => {
            result.ok = false;
            result.errors.push(format!(
                "Elevated Windows Firewall helper exited with code {code}"
            ));
        }
        Err(err) => {
            result.ok = false;
            result.errors.push(err);
        }
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

fn ufw_line_allows_port(line: &str, port: u16, proto: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("Status:")
        || trimmed.starts_with("To")
        || trimmed.starts_with("--")
    {
        return false;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 2 {
        return false;
    }
    let target_with_proto = format!("{port}/{proto}");
    let target_bare = port.to_string();
    let to_tok = tokens[0];
    let matches_target = to_tok.eq_ignore_ascii_case(&target_with_proto) || to_tok == target_bare;
    if !matches_target {
        return false;
    }
    // Skip IPv6-only lines such as `4713/tcp (v6) ALLOW Anywhere (v6)` so IPv4 rules are verified.
    if tokens
        .get(1)
        .is_some_and(|t| t.eq_ignore_ascii_case("(v6)"))
    {
        return false;
    }
    let action_idx = 1;
    if !tokens[action_idx].eq_ignore_ascii_case("ALLOW") {
        return false;
    }
    if tokens
        .get(action_idx + 1)
        .is_some_and(|t| t.eq_ignore_ascii_case("OUT") || t.eq_ignore_ascii_case("FWD"))
    {
        return false;
    }
    true
}

pub fn parse_ufw_ports_allowed(
    stdout: &str,
    audio_port: u16,
    discovery_port: u16,
    include_mdns: bool,
) -> Option<bool> {
    if !stdout.to_lowercase().contains("status: active") {
        return None;
    }
    let mut audio_ok = false;
    let mut discovery_ok = false;
    let mut mdns_ok = !include_mdns;
    for line in stdout.lines() {
        if ufw_line_allows_port(line, audio_port, "tcp") {
            audio_ok = true;
        }
        if ufw_line_allows_port(line, discovery_port, "udp") {
            discovery_ok = true;
        }
        if include_mdns && ufw_line_allows_port(line, MDNS_PORT, "udp") {
            mdns_ok = true;
        }
    }
    Some(audio_ok && discovery_ok && mdns_ok)
}

pub fn parse_firewall_cmd_query_output(returncode: i32, stdout: &str) -> Option<bool> {
    let trimmed = stdout.trim();
    if returncode == 0 && trimmed.eq_ignore_ascii_case("yes") {
        Some(true)
    } else if trimmed.eq_ignore_ascii_case("no") {
        Some(false)
    } else {
        None
    }
}

fn check_firewalld_ports_allowed(
    runner: &dyn ProcessRunner,
    audio_port: u16,
    discovery_port: u16,
    include_mdns: bool,
) -> Option<bool> {
    let query_port = |spec: String| -> Option<bool> {
        let args = vec!["firewall-cmd".to_string(), format!("--query-port={spec}")];
        let res = runner.run(&args, None, Duration::from_secs(5)).ok()?;
        parse_firewall_cmd_query_output(res.returncode, &res.stdout)
    };

    let audio_ok = query_port(format!("{audio_port}/tcp"))?;
    if !audio_ok {
        return Some(false);
    }
    let discovery_ok = query_port(format!("{discovery_port}/udp"))?;
    if !discovery_ok {
        return Some(false);
    }
    if include_mdns {
        let mdns_port_ok = query_port(format!("{MDNS_PORT}/udp"))?;
        if !mdns_port_ok {
            let svc_args = vec![
                "firewall-cmd".to_string(),
                "--query-service=mdns".to_string(),
            ];
            let svc_ok = runner
                .run(&svc_args, None, Duration::from_secs(5))
                .ok()
                .and_then(|res| parse_firewall_cmd_query_output(res.returncode, &res.stdout))?;
            if !svc_ok {
                return Some(false);
            }
        }
    }
    Some(true)
}

pub fn parse_netsh_rule_allowed(returncode: i32, stdout: &str, stderr: &str) -> Option<bool> {
    let combined = format!("{stdout}\n{stderr}");
    if is_netsh_no_matching_rules(&combined) {
        return Some(false);
    }
    if returncode != 0 {
        return None;
    }
    if stdout.trim().is_empty() {
        return None;
    }

    let mut enabled: Option<bool> = None;
    let mut action_allow: Option<bool> = None;
    let mut direction_in: Option<bool> = None;

    for raw_line in stdout.lines() {
        let Some((key, val)) = raw_line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        if key.eq_ignore_ascii_case("Enabled") {
            if val.eq_ignore_ascii_case("Yes") {
                enabled = Some(true);
            } else if val.eq_ignore_ascii_case("No") {
                enabled = Some(false);
            }
        } else if key.eq_ignore_ascii_case("Action") {
            if val.eq_ignore_ascii_case("Allow") {
                action_allow = Some(true);
            } else if val.eq_ignore_ascii_case("Block") {
                action_allow = Some(false);
            }
        } else if key.eq_ignore_ascii_case("Direction") {
            if val.eq_ignore_ascii_case("In") {
                direction_in = Some(true);
            } else if val.eq_ignore_ascii_case("Out") {
                direction_in = Some(false);
            }
        }
    }

    if enabled == Some(false) || action_allow == Some(false) || direction_in == Some(false) {
        return Some(false);
    }
    if enabled == Some(true) && action_allow == Some(true) {
        return Some(true);
    }
    // Fallback for localized Windows output where `netsh show rule` succeeded with exit code 0.
    Some(true)
}

fn check_windows_ports_allowed(
    runner: &dyn ProcessRunner,
    audio_port: u16,
    discovery_port: u16,
    include_mdns: bool,
) -> Option<bool> {
    let mut rule_names = vec![
        windows_audio_rule_name(audio_port),
        windows_discovery_rule_name(discovery_port),
    ];
    if include_mdns {
        rule_names.push(windows_mdns_rule_name());
    }
    for name in rule_names {
        let cmd = windows_show_rule_cmd(&name);
        let res = runner.run(&cmd, None, Duration::from_secs(5)).ok()?;
        let allowed = parse_netsh_rule_allowed(res.returncode, &res.stdout, &res.stderr)?;
        if !allowed {
            return Some(false);
        }
    }
    Some(true)
}

pub fn detect() -> FirewallState {
    detect_for_ports(DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, false)
}

pub fn detect_for_ports(audio_port: u16, discovery_port: u16, include_mdns: bool) -> FirewallState {
    detect_with_runner_for_ports(&SystemRunner, audio_port, discovery_port, include_mdns)
}

pub fn detect_with_runner(runner: &dyn ProcessRunner) -> FirewallState {
    detect_with_runner_for_ports(runner, DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, false)
}

pub fn detect_with_runner_for_ports(
    runner: &dyn ProcessRunner,
    audio_port: u16,
    discovery_port: u16,
    include_mdns: bool,
) -> FirewallState {
    detect_with_runner_and_ufw_conf(
        runner,
        Path::new("/etc/ufw/ufw.conf"),
        audio_port,
        discovery_port,
        include_mdns,
    )
}

fn detect_with_runner_and_ufw_conf(
    runner: &dyn ProcessRunner,
    ufw_conf_path: &Path,
    audio_port: u16,
    discovery_port: u16,
    include_mdns: bool,
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
                    let active = parse_windows_firewall_state(&res.stdout);
                    let ports_allowed = if active {
                        check_windows_ports_allowed(
                            runner,
                            audio_port,
                            discovery_port,
                            include_mdns,
                        )
                    } else {
                        None
                    };
                    return FirewallState {
                        kind: "windows-defender".to_string(),
                        active,
                        detail: if active {
                            "Windows Defender Firewall is active".to_string()
                        } else {
                            "Windows Defender Firewall is off".to_string()
                        },
                        ports_allowed,
                    };
                }
            }
        }
        return FirewallState {
            kind: "none".to_string(),
            active: false,
            detail: "Windows Defender Firewall state could not be determined".to_string(),
            ports_allowed: None,
        };
    }

    if runner.command_exists("ufw") {
        let args = vec!["ufw".to_string(), "status".to_string()];
        if let Ok(res) = runner.run(&args, None, Duration::from_secs(5)) {
            let output = format!("{}{}", res.stdout, res.stderr).to_lowercase();
            if output.contains("status: active") {
                let ports_allowed =
                    parse_ufw_ports_allowed(&res.stdout, audio_port, discovery_port, include_mdns);
                return FirewallState {
                    kind: "ufw".to_string(),
                    active: true,
                    detail: "ufw is active".to_string(),
                    ports_allowed,
                };
            }
            if output.contains("status: inactive") {
                return FirewallState {
                    kind: "ufw".to_string(),
                    active: false,
                    detail: "ufw is installed but inactive".to_string(),
                    ports_allowed: None,
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
                    ports_allowed: None,
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
                    ports_allowed: None,
                };
            }
        }
        return FirewallState {
            kind: "ufw".to_string(),
            active: false,
            detail: "ufw state could not be determined".to_string(),
            ports_allowed: None,
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
                let ports_allowed =
                    check_firewalld_ports_allowed(runner, audio_port, discovery_port, include_mdns);
                return FirewallState {
                    kind: "firewalld".to_string(),
                    active: true,
                    detail: "firewalld is active".to_string(),
                    ports_allowed,
                };
            }
            return FirewallState {
                kind: "firewalld".to_string(),
                active: false,
                detail: "firewalld is installed but inactive".to_string(),
                ports_allowed: None,
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
            ports_allowed: None,
        };
    }

    FirewallState::default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxElevation {
    Direct,
    SudoInteractive,
    SudoNonInteractive,
    PkexecBatch,
}

fn select_linux_elevation(
    runner: &dyn ProcessRunner,
    interactive: bool,
    is_admin_user: bool,
    stdin_is_tty: bool,
) -> LinuxElevation {
    if is_admin_user {
        return LinuxElevation::Direct;
    }
    if interactive && stdin_is_tty {
        if runner.command_exists("sudo") {
            return LinuxElevation::SudoInteractive;
        }
        if runner.command_exists("pkexec") {
            return LinuxElevation::PkexecBatch;
        }
        return LinuxElevation::Direct;
    }
    // Non-TTY / GUI / AppImage launch: prefer a single batched Polkit prompt over `sudo -n`.
    if runner.command_exists("pkexec") {
        return LinuxElevation::PkexecBatch;
    }
    if runner.command_exists("sudo") {
        return LinuxElevation::SudoNonInteractive;
    }
    LinuxElevation::Direct
}

fn shell_quote_posix(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b',')
        })
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn build_pkexec_batch_command(commands: &[Vec<String>]) -> Option<Vec<String>> {
    if commands.is_empty() {
        return None;
    }
    let script = commands
        .iter()
        .map(|cmd| {
            cmd.iter()
                .map(|a| shell_quote_posix(a))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" && ");
    Some(vec![
        "pkexec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ])
}

fn apply_linux_elevation(
    raw_commands: Vec<Vec<String>>,
    elevation: LinuxElevation,
) -> Vec<Vec<String>> {
    match elevation {
        LinuxElevation::Direct => raw_commands,
        LinuxElevation::SudoInteractive => raw_commands
            .into_iter()
            .map(|mut cmd| {
                cmd.insert(0, "sudo".to_string());
                cmd
            })
            .collect(),
        LinuxElevation::SudoNonInteractive => raw_commands
            .into_iter()
            .map(|mut cmd| {
                cmd.insert(0, "-n".to_string());
                cmd.insert(0, "sudo".to_string());
                cmd
            })
            .collect(),
        LinuxElevation::PkexecBatch => build_pkexec_batch_command(&raw_commands)
            .into_iter()
            .collect(),
    }
}

fn execute_firewall_commands_with_runner(
    runner: &dyn ProcessRunner,
    result: &mut FirewallResult,
    interactive: bool,
    is_close: bool,
    use_system_interactive: bool,
) {
    #[cfg(windows)]
    if result.firewall.kind == "windows-defender" {
        let _ = (runner, use_system_interactive);
        execute_windows_firewall_commands(result, interactive, is_close);
        if result.ok && result.ran {
            result.firewall.ports_allowed = Some(!is_close);
        }
        return;
    }

    result.ran = true;
    for cmd in &result.commands {
        let first = cmd.first().map(|s| s.as_str());
        let is_interactive_sudo =
            interactive && first == Some("sudo") && !cmd.iter().any(|a| a == "-n");
        let is_pkexec = first == Some("pkexec");
        let run_res = if use_system_interactive && is_interactive_sudo {
            SystemRunner.run_interactive(cmd, None, Duration::from_secs(60))
        } else if is_pkexec || is_interactive_sudo {
            runner.run(cmd, None, Duration::from_secs(60))
        } else {
            runner.run(cmd, None, Duration::from_secs(15))
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
    if result.ok && result.ran {
        result.firewall.ports_allowed = Some(!is_close);
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

/// Non-interactive variant for background daemons/IPC: uses `pkexec` batch elevation
/// when available on Linux (GUI/Polkit), otherwise falls back to `sudo -n` so it
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
    open_ports_with_runner(
        &SystemRunner,
        audio_port,
        discovery_port,
        dry_run,
        include_mdns,
        interactive,
        is_admin(),
        std::io::stdin().is_terminal(),
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn open_ports_with_runner(
    runner: &dyn ProcessRunner,
    audio_port: u16,
    discovery_port: u16,
    dry_run: bool,
    include_mdns: bool,
    interactive: bool,
    is_admin_user: bool,
    stdin_is_tty: bool,
    use_system_interactive: bool,
) -> FirewallResult {
    let state = detect_with_runner_for_ports(runner, audio_port, discovery_port, include_mdns);
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

    let elevation = select_linux_elevation(runner, interactive, is_admin_user, stdin_is_tty);
    match state.kind.as_str() {
        "ufw" => {
            let mut raw = vec![
                vec!["ufw".into(), "allow".into(), format!("{audio_port}/tcp")],
                vec![
                    "ufw".into(),
                    "allow".into(),
                    format!("{discovery_port}/udp"),
                ],
            ];
            if include_mdns {
                raw.push(vec![
                    "ufw".into(),
                    "allow".into(),
                    format!("{MDNS_PORT}/udp"),
                ]);
            }
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "firewalld" => {
            let mut raw = vec![
                vec![
                    "firewall-cmd".into(),
                    format!("--add-port={audio_port}/tcp"),
                    "--permanent".into(),
                ],
                vec![
                    "firewall-cmd".into(),
                    format!("--add-port={discovery_port}/udp"),
                    "--permanent".into(),
                ],
            ];
            if include_mdns {
                raw.push(vec![
                    "firewall-cmd".into(),
                    format!("--add-port={MDNS_PORT}/udp"),
                    "--permanent".into(),
                ]);
                raw.push(vec![
                    "firewall-cmd".into(),
                    "--add-service=mdns".into(),
                    "--permanent".into(),
                ]);
            }
            raw.push(vec!["firewall-cmd".into(), "--reload".into()]);
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "windows-defender" => {
            result.commands.extend(build_windows_open_commands(
                audio_port,
                discovery_port,
                include_mdns,
            ));
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

    execute_firewall_commands_with_runner(
        runner,
        &mut result,
        interactive,
        false,
        use_system_interactive,
    );
    result
}

pub fn close_ports(audio_port: u16, discovery_port: u16, dry_run: bool) -> FirewallResult {
    close_ports_with_runner(
        &SystemRunner,
        audio_port,
        discovery_port,
        dry_run,
        true,
        is_admin(),
        std::io::stdin().is_terminal(),
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn close_ports_with_runner(
    runner: &dyn ProcessRunner,
    audio_port: u16,
    discovery_port: u16,
    dry_run: bool,
    interactive: bool,
    is_admin_user: bool,
    stdin_is_tty: bool,
    use_system_interactive: bool,
) -> FirewallResult {
    let state = detect_with_runner_for_ports(runner, audio_port, discovery_port, false);
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
    let elevation = select_linux_elevation(runner, interactive, is_admin_user, stdin_is_tty);
    match state.kind.as_str() {
        "ufw" => {
            let raw = vec![
                vec![
                    "ufw".into(),
                    "delete".into(),
                    "allow".into(),
                    format!("{audio_port}/tcp"),
                ],
                vec![
                    "ufw".into(),
                    "delete".into(),
                    "allow".into(),
                    format!("{discovery_port}/udp"),
                ],
            ];
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "firewalld" => {
            let raw = vec![
                vec![
                    "firewall-cmd".into(),
                    format!("--remove-port={audio_port}/tcp"),
                    "--permanent".into(),
                ],
                vec![
                    "firewall-cmd".into(),
                    format!("--remove-port={discovery_port}/udp"),
                    "--permanent".into(),
                ],
                vec!["firewall-cmd".into(), "--reload".into()],
            ];
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "windows-defender" => {
            result
                .commands
                .extend(build_windows_close_commands(audio_port, discovery_port));
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
    execute_firewall_commands_with_runner(
        runner,
        &mut result,
        interactive,
        true,
        use_system_interactive,
    );
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

        let st_yes = detect_with_runner_and_ufw_conf(
            &runner,
            &conf_yes,
            DEFAULT_AUDIO_PORT,
            DEFAULT_DISCOVERY_PORT,
            false,
        );
        assert_eq!(st_yes.kind, "ufw");
        assert!(st_yes.active);
        assert_eq!(st_yes.ports_allowed, None);

        let st_no = detect_with_runner_and_ufw_conf(
            &runner,
            &conf_no,
            DEFAULT_AUDIO_PORT,
            DEFAULT_DISCOVERY_PORT,
            false,
        );
        assert_eq!(st_no.kind, "ufw");
        assert!(!st_no.active);
        assert_eq!(st_no.ports_allowed, None);

        let st_systemd = detect_with_runner_and_ufw_conf(
            &runner,
            &conf_missing,
            DEFAULT_AUDIO_PORT,
            DEFAULT_DISCOVERY_PORT,
            false,
        );
        assert_eq!(st_systemd.kind, "ufw");
        assert!(st_systemd.active);
        assert_eq!(st_systemd.ports_allowed, None);
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

    #[test]
    fn test_parse_windows_firewall_state() {
        let all_on = "\
Domain Profile Settings:
----------------------------------------------------------------------
State                                 ON

Private Profile Settings:
----------------------------------------------------------------------
State                                 ON

Public Profile Settings:
----------------------------------------------------------------------
State                                 ON
Ok.
";
        assert!(parse_windows_firewall_state(all_on));

        let all_off = "\
Domain Profile Settings:
----------------------------------------------------------------------
State                                 OFF
Connection                            Block
Private Profile Settings:
----------------------------------------------------------------------
State                                 OFF
Public Profile Settings:
----------------------------------------------------------------------
State                                 OFF
Ok.
";
        assert!(!parse_windows_firewall_state(all_off));

        let mixed = "\
Domain Profile Settings:
State                                 OFF
Private Profile Settings:
State                                 ON
";
        assert!(parse_windows_firewall_state(mixed));
    }

    #[test]
    fn test_build_windows_open_commands_idempotent_and_scoped() {
        let cmds_no_mdns = build_windows_open_commands(4713, 47777, false);
        assert_eq!(cmds_no_mdns.len(), 4);
        assert!(is_netsh_delete_rule(&cmds_no_mdns[0]));
        assert_eq!(
            extract_netsh_rule_name(&cmds_no_mdns[0]),
            Some("SoundHop Audio (4713/tcp)")
        );
        assert!(!is_netsh_delete_rule(&cmds_no_mdns[1]));
        assert!(cmds_no_mdns[1].contains(&"profile=private,domain".to_string()));
        assert!(cmds_no_mdns[1].contains(&"protocol=TCP".to_string()));
        assert!(cmds_no_mdns[1].contains(&"localport=4713".to_string()));

        assert!(is_netsh_delete_rule(&cmds_no_mdns[2]));
        assert_eq!(
            extract_netsh_rule_name(&cmds_no_mdns[2]),
            Some("SoundHop Discovery (47777/udp)")
        );
        assert!(cmds_no_mdns[3].contains(&"profile=private,domain".to_string()));
        assert!(cmds_no_mdns[3].contains(&"protocol=UDP".to_string()));
        assert!(cmds_no_mdns[3].contains(&"localport=47777".to_string()));

        let cmds_with_mdns = build_windows_open_commands(4713, 47777, true);
        assert_eq!(cmds_with_mdns.len(), 6);
        assert!(is_netsh_delete_rule(&cmds_with_mdns[4]));
        assert_eq!(
            extract_netsh_rule_name(&cmds_with_mdns[4]),
            Some("SoundHop mDNS (5353/udp)")
        );
        assert!(cmds_with_mdns[5].contains(&"profile=private,domain".to_string()));
        assert!(cmds_with_mdns[5].contains(&"protocol=UDP".to_string()));
        assert!(cmds_with_mdns[5].contains(&format!("localport={MDNS_PORT}")));
    }

    #[test]
    fn test_build_windows_close_commands_includes_mdns() {
        let cmds = build_windows_close_commands(4713, 47777);
        assert_eq!(cmds.len(), 3);
        assert!(cmds.iter().all(|c| is_netsh_delete_rule(c)));
        let names: Vec<Option<&str>> = cmds.iter().map(|c| extract_netsh_rule_name(c)).collect();
        assert_eq!(
            names,
            vec![
                Some("SoundHop Audio (4713/tcp)"),
                Some("SoundHop Discovery (47777/udp)"),
                Some("SoundHop mDNS (5353/udp)"),
            ]
        );
    }

    #[test]
    fn test_build_windows_elevated_cmd_params_open_and_close() {
        let open_cmds = build_windows_open_commands(4713, 47777, true);
        let open_params = build_windows_elevated_cmd_params(&open_cmds, false);
        assert!(open_params.starts_with("/C \""));
        assert!(open_params.ends_with('"'));
        assert!(open_params.contains("name=\"SoundHop Audio (4713/tcp)\""));
        assert!(open_params.contains("name=\"SoundHop Discovery (47777/udp)\""));
        assert!(open_params.contains("name=\"SoundHop mDNS (5353/udp)\""));
        assert!(open_params.contains("profile=private,domain"));
        assert!(open_params.contains(" >nul 2>&1 & netsh advfirewall firewall add rule "));
        assert!(open_params.contains(") && ("));

        let close_cmds = build_windows_close_commands(4713, 47777);
        let close_params = build_windows_elevated_cmd_params(&close_cmds, true);
        assert!(close_params.starts_with("/C \""));
        assert!(close_params.ends_with("& exit /b 0\""));
        assert!(close_params.contains("show rule name=\"SoundHop mDNS (5353/udp)\""));
        assert!(close_params.contains("delete rule name=\"SoundHop mDNS (5353/udp)\""));
    }

    #[test]
    fn test_is_netsh_no_matching_rules() {
        assert!(is_netsh_no_matching_rules(
            "No rules match the specified criteria.\r\n"
        ));
        assert!(!is_netsh_no_matching_rules(
            "The requested operation requires elevation (Run as administrator)."
        ));
    }

    #[test]
    fn test_parse_ufw_ports_allowed() {
        let ufw_open = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   ALLOW       Anywhere
47777/udp                  ALLOW IN    Anywhere
5353/udp                   ALLOW       Anywhere
4713/tcp (v6)              ALLOW       Anywhere (v6)
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_open, 4713, 47777, false),
            Some(true)
        );
        assert_eq!(
            parse_ufw_ports_allowed(ufw_open, 4713, 47777, true),
            Some(true)
        );

        let ufw_missing_mdns = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   ALLOW       Anywhere
47777/udp                  ALLOW       Anywhere
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_missing_mdns, 4713, 47777, false),
            Some(true)
        );
        assert_eq!(
            parse_ufw_ports_allowed(ufw_missing_mdns, 4713, 47777, true),
            Some(false)
        );

        let ufw_denied_or_out = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   DENY        Anywhere
47777/udp                  ALLOW OUT   Anywhere
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_denied_or_out, 4713, 47777, false),
            Some(false)
        );

        let ufw_v6_only = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp (v6)              ALLOW       Anywhere (v6)
47777/udp (v6)             ALLOW       Anywhere (v6)
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_v6_only, 4713, 47777, false),
            Some(false)
        );

        assert_eq!(
            parse_ufw_ports_allowed("Status: inactive\n", 4713, 47777, false),
            None
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn test_detect_firewalld_ports_allowed_and_missing() {
        let runner = MockRunner::new(|args, _| {
            if args.first().map(|s| s.as_str()) == Some("systemctl") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            if args.first().map(|s| s.as_str()) == Some("firewall-cmd") {
                let query = args.get(1).map(|s| s.as_str()).unwrap_or("");
                if query == "--query-port=4713/tcp"
                    || query == "--query-port=47777/udp"
                    || query == "--query-service=mdns"
                {
                    return Ok(CommandResult {
                        args: args.to_vec(),
                        returncode: 0,
                        stdout: "yes\n".to_string(),
                        stderr: String::new(),
                    });
                }
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 1,
                    stdout: "no\n".to_string(),
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
            .insert("firewall-cmd".to_string());

        let st_with_mdns = detect_with_runner_for_ports(&runner, 4713, 47777, true);
        assert_eq!(st_with_mdns.kind, "firewalld");
        assert!(st_with_mdns.active);
        assert_eq!(st_with_mdns.ports_allowed, Some(true));

        let st_missing_custom = detect_with_runner_for_ports(&runner, 4799, 47777, false);
        assert_eq!(st_missing_custom.kind, "firewalld");
        assert!(st_missing_custom.active);
        assert_eq!(st_missing_custom.ports_allowed, Some(false));
    }

    #[test]
    fn test_parse_netsh_rule_allowed_and_windows_port_check() {
        let allowed_out = "\
Rule Name:                            SoundHop Audio (4713/tcp)
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Domain,Private
Action:                               Allow
Ok.
";
        assert_eq!(parse_netsh_rule_allowed(0, allowed_out, ""), Some(true));

        let disabled_out = "\
Rule Name:                            SoundHop Audio (4713/tcp)
Enabled:                              No
Direction:                            In
Action:                               Allow
";
        assert_eq!(parse_netsh_rule_allowed(0, disabled_out, ""), Some(false));

        let blocked_out = "\
Rule Name:                            SoundHop Audio (4713/tcp)
Enabled:                              Yes
Direction:                            In
Action:                               Block
";
        assert_eq!(parse_netsh_rule_allowed(0, blocked_out, ""), Some(false));

        assert_eq!(
            parse_netsh_rule_allowed(1, "No rules match the specified criteria.\r\n", ""),
            Some(false)
        );
        assert_eq!(parse_netsh_rule_allowed(1, "", "Access is denied."), None);

        let runner = MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            if joined.contains("SoundHop Audio (4713/tcp)")
                || joined.contains("SoundHop Discovery (47777/udp)")
            {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: allowed_out.to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 1,
                stdout: "No rules match the specified criteria.\r\n".to_string(),
                stderr: String::new(),
            })
        });
        assert_eq!(
            check_windows_ports_allowed(&runner, 4713, 47777, false),
            Some(true)
        );
        assert_eq!(
            check_windows_ports_allowed(&runner, 4713, 47777, true),
            Some(false)
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn test_linux_pkexec_batch_elevation_when_non_tty() {
        use std::sync::{Arc, Mutex};

        let invoked: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let invoked_c = Arc::clone(&invoked);

        let runner = MockRunner::new(move |args, _| {
            invoked_c.lock().unwrap().push(args.to_vec());
            if args.first().map(|s| s.as_str()) == Some("ufw") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "Status: active\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: "Rules updated\n".to_string(),
                stderr: String::new(),
            })
        });
        {
            let mut cmds = runner.available_commands.lock().unwrap();
            cmds.insert("ufw".to_string());
            cmds.insert("pkexec".to_string());
            cmds.insert("sudo".to_string());
        }

        // Non-TTY (GUI/AppImage launch): prefers single batched `pkexec sh -c ...` instead of `sudo -n`.
        let res = open_ports_with_runner(
            &runner, 4713, 47777, false, true, false, false, false, false,
        );
        assert!(res.ok);
        assert!(res.ran);
        assert_eq!(res.firewall.ports_allowed, Some(true));
        assert_eq!(res.commands.len(), 1);
        assert_eq!(res.commands[0][0], "pkexec");
        assert_eq!(res.commands[0][1], "sh");
        assert_eq!(res.commands[0][2], "-c");
        assert_eq!(
            res.commands[0][3],
            "ufw allow 4713/tcp && ufw allow 47777/udp && ufw allow 5353/udp"
        );

        let calls = invoked.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| c.first().map(|s| s.as_str()) == Some("pkexec")),
            "expected pkexec invocation; got: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.first().map(|s| s.as_str()) == Some("sudo")),
            "must not use sudo -n when pkexec is available in non-TTY mode; got: {calls:?}"
        );

        // TTY + interactive: still uses interactive `sudo`.
        let tty_res =
            open_ports_with_runner(&runner, 4713, 47777, true, false, true, false, true, false);
        assert_eq!(tty_res.commands.len(), 2);
        assert_eq!(
            tty_res.commands[0],
            vec!["sudo", "ufw", "allow", "4713/tcp"]
        );

        // Non-TTY without `pkexec`: falls back to `sudo -n`.
        runner.available_commands.lock().unwrap().remove("pkexec");
        let fallback_res = open_ports_with_runner(
            &runner, 4713, 47777, true, false, false, false, false, false,
        );
        assert_eq!(fallback_res.commands.len(), 2);
        assert_eq!(
            fallback_res.commands[0],
            vec!["sudo", "-n", "ufw", "allow", "4713/tcp"]
        );
    }
}
