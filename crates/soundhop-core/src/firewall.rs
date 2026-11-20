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

#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg_attr(not(windows), allow(dead_code))]
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
                    let active = parse_windows_firewall_state(&res.stdout);
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

fn execute_firewall_commands(result: &mut FirewallResult, interactive: bool, is_close: bool) {
    #[cfg(windows)]
    if result.firewall.kind == "windows-defender" {
        execute_windows_firewall_commands(result, interactive, is_close);
        return;
    }
    #[cfg(not(windows))]
    let _ = is_close;

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

    execute_firewall_commands(&mut result, interactive, false);
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
    execute_firewall_commands(&mut result, true, true);
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
}
