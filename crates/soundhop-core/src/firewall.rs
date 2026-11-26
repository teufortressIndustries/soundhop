//! Firewall inspection and rule helpers for UFW, firewalld, nftables, and Windows Defender.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use crate::config::{is_paired_security_mode, Config, DEFAULT_RTP_PORT};
use crate::constants::{
    DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT, DEFAULT_WEBRTC_PORT, DEFAULT_WEB_PORT, MDNS_PORT,
};
use crate::pairing::DEFAULT_PAIR_PORT;
use crate::transport::DEFAULT_V3_PORT;
use crate::util::{ProcessRunner, SystemRunner};

/// Transport protocol for a firewall port rule.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

impl PortProtocol {
    pub fn as_lower(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    pub fn as_upper(self) -> &'static str {
        match self {
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
        }
    }
}

/// Single firewall port specification (port, protocol, and display label).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallPortSpec {
    pub port: u16,
    pub protocol: PortProtocol,
    pub label: String,
    #[serde(default)]
    pub is_mdns: bool,
}

impl FirewallPortSpec {
    pub fn new(port: u16, protocol: PortProtocol, label: impl Into<String>) -> Self {
        Self {
            port,
            protocol,
            label: label.into(),
            is_mdns: false,
        }
    }

    pub fn mdns() -> Self {
        Self {
            port: MDNS_PORT,
            protocol: PortProtocol::Udp,
            label: "mDNS".to_string(),
            is_mdns: true,
        }
    }

    /// Canonical `port/proto` spec string (e.g., `"4714/tcp"`, `"4716/udp"`).
    pub fn port_proto(&self) -> String {
        format!("{}/{}", self.port, self.protocol.as_lower())
    }

    /// Windows Defender Firewall rule display name (e.g., `"SoundHop Audio (4713/tcp)"`).
    pub fn windows_rule_name(&self) -> String {
        format!("SoundHop {} ({})", self.label, self.port_proto())
    }
}

/// Full set of SoundHop firewall ports derived from [`Config`] or explicit settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallPorts {
    /// Legacy PulseAudio TCP audio port (`None` when omitted in paired/PSK security mode).
    pub legacy_audio_tcp: Option<u16>,
    /// Control / SHT3 TCP port (`cfg.sharing.v3_port`, default `4714`).
    pub control_tcp: u16,
    /// Pairing v2 interactive handshake TCP port (`cfg.sharing.pair_port`, default `4715`).
    pub pair_tcp: u16,
    /// Encrypted RTP/SRTP media UDP port (`cfg.sharing.rtp_port`, default `4716`).
    pub rtp_udp: u16,
    /// UDP broadcast discovery port (`cfg.discovery.port`, default `47777`).
    pub discovery_udp: u16,
    /// Whether to include mDNS (`5353/udp`).
    pub include_mdns: bool,
    /// Web Broadcast HTTP/WS TCP port (`None` when Web Broadcast is disabled).
    #[serde(default)]
    pub web_tcp: Option<u16>,
    /// Web Broadcast WebRTC ICE-Lite UDP port (`None` when Web Broadcast is disabled).
    #[serde(default)]
    pub webrtc_udp: Option<u16>,
}

impl Default for FirewallPorts {
    fn default() -> Self {
        Self {
            legacy_audio_tcp: Some(DEFAULT_AUDIO_PORT),
            control_tcp: DEFAULT_V3_PORT,
            pair_tcp: DEFAULT_PAIR_PORT,
            rtp_udp: DEFAULT_RTP_PORT,
            discovery_udp: DEFAULT_DISCOVERY_PORT,
            include_mdns: false,
            web_tcp: None,
            webrtc_udp: None,
        }
    }
}

impl FirewallPorts {
    /// Build the required firewall port set from a SoundHop [`Config`].
    ///
    /// Omits the legacy PulseAudio TCP port (`sharing.port`) when paired/PSK security
    /// mode is active, since paired mode does not expose unauthenticated PA-TCP.
    pub fn from_config(cfg: &Config) -> Self {
        Self::from_config_with_web(
            cfg,
            cfg.sharing.web_enabled,
            cfg.sharing.web_port,
            cfg.sharing.webrtc_port,
        )
    }

    /// Build the required firewall port set from a SoundHop [`Config`] while overriding
    /// whether Web Broadcast ports are active (e.g., for ad-hoc starts).
    pub fn from_config_with_web(
        cfg: &Config,
        web_active: bool,
        web_port: u16,
        webrtc_port: u16,
    ) -> Self {
        let legacy_audio_tcp = if is_paired_security_mode(&cfg.sharing.security) {
            None
        } else {
            Some(cfg.sharing.port)
        };
        Self {
            legacy_audio_tcp,
            control_tcp: cfg.sharing.v3_port,
            pair_tcp: cfg.sharing.pair_port,
            rtp_udp: cfg.sharing.rtp_port,
            discovery_udp: cfg.discovery.port,
            include_mdns: cfg.discovery.mdns,
            web_tcp: web_active.then_some(web_port),
            webrtc_udp: web_active.then_some(webrtc_port),
        }
    }

    /// Ordered, deduplicated list of firewall port specifications to inspect or open.
    pub fn specs(&self) -> Vec<FirewallPortSpec> {
        let mut out = Vec::new();
        let mut push_unique = |spec: FirewallPortSpec| {
            if spec.port == 0 {
                return;
            }
            if !out.iter().any(|existing: &FirewallPortSpec| {
                existing.port == spec.port && existing.protocol == spec.protocol
            }) {
                out.push(spec);
            }
        };
        if let Some(port) = self.legacy_audio_tcp {
            push_unique(FirewallPortSpec::new(port, PortProtocol::Tcp, "Audio"));
        }
        push_unique(FirewallPortSpec::new(
            self.control_tcp,
            PortProtocol::Tcp,
            "Control",
        ));
        push_unique(FirewallPortSpec::new(
            self.pair_tcp,
            PortProtocol::Tcp,
            "Pairing",
        ));
        push_unique(FirewallPortSpec::new(
            self.rtp_udp,
            PortProtocol::Udp,
            "RTP",
        ));
        push_unique(FirewallPortSpec::new(
            self.discovery_udp,
            PortProtocol::Udp,
            "Discovery",
        ));
        if self.include_mdns {
            push_unique(FirewallPortSpec::mdns());
        }
        if let Some(port) = self.web_tcp {
            push_unique(FirewallPortSpec::new(port, PortProtocol::Tcp, "Web"));
        }
        if let Some(port) = self.webrtc_udp {
            push_unique(FirewallPortSpec::new(port, PortProtocol::Udp, "WebRTC"));
        }
        out
    }

    /// Port specifications to delete on Windows Defender cleanup (always includes the
    /// SoundHop-scoped mDNS and Web Broadcast rules so previously created rules are cleaned up).
    pub fn windows_close_specs(&self) -> Vec<FirewallPortSpec> {
        let mut copy = self.clone();
        copy.include_mdns = true;
        copy.web_tcp.get_or_insert(DEFAULT_WEB_PORT);
        copy.webrtc_udp.get_or_insert(DEFAULT_WEBRTC_PORT);
        copy.specs()
    }
}

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

fn build_windows_open_commands(specs: &[FirewallPortSpec]) -> Vec<Vec<String>> {
    let mut cmds = Vec::with_capacity(specs.len() * 2);
    for spec in specs {
        let name = spec.windows_rule_name();
        cmds.push(windows_delete_rule_cmd(&name));
        cmds.push(windows_add_rule_cmd(
            &name,
            spec.protocol.as_upper(),
            spec.port,
        ));
    }
    cmds
}

fn build_windows_close_commands(specs: &[FirewallPortSpec]) -> Vec<Vec<String>> {
    specs
        .iter()
        .map(|spec| windows_delete_rule_cmd(&spec.windows_rule_name()))
        .collect()
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

pub fn parse_ufw_ports_allowed(stdout: &str, specs: &[FirewallPortSpec]) -> Option<bool> {
    if !stdout.to_lowercase().contains("status: active") {
        return None;
    }
    let lines: Vec<&str> = stdout.lines().collect();
    let all_allowed = specs.iter().all(|spec| {
        lines
            .iter()
            .any(|line| ufw_line_allows_port(line, spec.port, spec.protocol.as_lower()))
    });
    Some(all_allowed)
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
    specs: &[FirewallPortSpec],
) -> Option<bool> {
    let query_port = |spec: String| -> Option<bool> {
        let args = vec!["firewall-cmd".to_string(), format!("--query-port={spec}")];
        let res = runner.run(&args, None, Duration::from_secs(5)).ok()?;
        parse_firewall_cmd_query_output(res.returncode, &res.stdout)
    };

    for spec in specs {
        let port_ok = query_port(spec.port_proto())?;
        if !port_ok {
            if spec.is_mdns {
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
            } else {
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
    specs: &[FirewallPortSpec],
) -> Option<bool> {
    for spec in specs {
        let name = spec.windows_rule_name();
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
    detect_for_ports(&FirewallPorts::default())
}

pub fn detect_for_ports(ports: &FirewallPorts) -> FirewallState {
    detect_with_runner_for_ports(&SystemRunner, ports)
}

pub fn detect_for_config(cfg: &Config) -> FirewallState {
    detect_for_ports(&FirewallPorts::from_config(cfg))
}

pub fn detect_with_runner(runner: &dyn ProcessRunner) -> FirewallState {
    detect_with_runner_for_ports(runner, &FirewallPorts::default())
}

pub fn detect_with_runner_for_ports(
    runner: &dyn ProcessRunner,
    ports: &FirewallPorts,
) -> FirewallState {
    detect_with_runner_and_ufw_conf(runner, Path::new("/etc/ufw/ufw.conf"), ports)
}

fn detect_with_runner_and_ufw_conf(
    runner: &dyn ProcessRunner,
    ufw_conf_path: &Path,
    ports: &FirewallPorts,
) -> FirewallState {
    let specs = ports.specs();
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
                        check_windows_ports_allowed(runner, &specs)
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
                let ports_allowed = parse_ufw_ports_allowed(&res.stdout, &specs);
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
                let ports_allowed = check_firewalld_ports_allowed(runner, &specs);
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

pub fn open_ports(ports: &FirewallPorts, dry_run: bool) -> FirewallResult {
    open_ports_with_mode(ports, dry_run, true)
}

pub fn open_ports_for_config(cfg: &Config, dry_run: bool) -> FirewallResult {
    open_ports(&FirewallPorts::from_config(cfg), dry_run)
}

/// Non-interactive variant for background daemons/IPC: uses `pkexec` batch elevation
/// when available on Linux (GUI/Polkit), otherwise falls back to `sudo -n` so it
/// never blocks on a TTY password prompt.
pub fn open_ports_non_interactive(ports: &FirewallPorts, dry_run: bool) -> FirewallResult {
    open_ports_with_mode(ports, dry_run, false)
}

pub fn open_ports_for_config_non_interactive(cfg: &Config, dry_run: bool) -> FirewallResult {
    open_ports_non_interactive(&FirewallPorts::from_config(cfg), dry_run)
}

fn open_ports_with_mode(ports: &FirewallPorts, dry_run: bool, interactive: bool) -> FirewallResult {
    open_ports_with_runner(
        &SystemRunner,
        ports,
        dry_run,
        interactive,
        is_admin(),
        std::io::stdin().is_terminal(),
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn open_ports_with_runner(
    runner: &dyn ProcessRunner,
    ports: &FirewallPorts,
    dry_run: bool,
    interactive: bool,
    is_admin_user: bool,
    stdin_is_tty: bool,
    use_system_interactive: bool,
) -> FirewallResult {
    let state = detect_with_runner_for_ports(runner, ports);
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

    let specs = ports.specs();
    let elevation = select_linux_elevation(runner, interactive, is_admin_user, stdin_is_tty);
    match state.kind.as_str() {
        "ufw" => {
            let raw: Vec<Vec<String>> = specs
                .iter()
                .map(|spec| vec!["ufw".into(), "allow".into(), spec.port_proto()])
                .collect();
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "firewalld" => {
            let mut raw = Vec::new();
            for spec in &specs {
                raw.push(vec![
                    "firewall-cmd".into(),
                    format!("--add-port={}", spec.port_proto()),
                    "--permanent".into(),
                ]);
                if spec.is_mdns {
                    raw.push(vec![
                        "firewall-cmd".into(),
                        "--add-service=mdns".into(),
                        "--permanent".into(),
                    ]);
                }
            }
            raw.push(vec!["firewall-cmd".into(), "--reload".into()]);
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "windows-defender" => {
            result.commands.extend(build_windows_open_commands(&specs));
        }
        _ => {
            let mut lines = vec!["nftables detected. Add these rules yourself:".to_string()];
            for spec in &specs {
                lines.push(format!(
                    "  nft add rule inet filter input {} dport {} accept",
                    spec.protocol.as_lower(),
                    spec.port
                ));
            }
            result.note = lines.join("\n");
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

pub fn close_ports(ports: &FirewallPorts, dry_run: bool) -> FirewallResult {
    close_ports_with_runner(
        &SystemRunner,
        ports,
        dry_run,
        true,
        is_admin(),
        std::io::stdin().is_terminal(),
        true,
    )
}

pub fn close_ports_for_config(cfg: &Config, dry_run: bool) -> FirewallResult {
    close_ports(&FirewallPorts::from_config(cfg), dry_run)
}

#[allow(clippy::too_many_arguments)]
fn close_ports_with_runner(
    runner: &dyn ProcessRunner,
    ports: &FirewallPorts,
    dry_run: bool,
    interactive: bool,
    is_admin_user: bool,
    stdin_is_tty: bool,
    use_system_interactive: bool,
) -> FirewallResult {
    let state = detect_with_runner_for_ports(runner, ports);
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
    let specs = ports.specs();
    let elevation = select_linux_elevation(runner, interactive, is_admin_user, stdin_is_tty);
    match state.kind.as_str() {
        "ufw" => {
            let raw: Vec<Vec<String>> = specs
                .iter()
                .map(|spec| {
                    vec![
                        "ufw".into(),
                        "delete".into(),
                        "allow".into(),
                        spec.port_proto(),
                    ]
                })
                .collect();
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "firewalld" => {
            let mut raw: Vec<Vec<String>> = specs
                .iter()
                .map(|spec| {
                    vec![
                        "firewall-cmd".into(),
                        format!("--remove-port={}", spec.port_proto()),
                        "--permanent".into(),
                    ]
                })
                .collect();
            raw.push(vec!["firewall-cmd".into(), "--reload".into()]);
            result.commands = apply_linux_elevation(raw, elevation);
        }
        "windows-defender" => {
            let close_specs = ports.windows_close_specs();
            result
                .commands
                .extend(build_windows_close_commands(&close_specs));
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

        let default_ports = FirewallPorts::default();

        let st_yes = detect_with_runner_and_ufw_conf(&runner, &conf_yes, &default_ports);
        assert_eq!(st_yes.kind, "ufw");
        assert!(st_yes.active);
        assert_eq!(st_yes.ports_allowed, None);

        let st_no = detect_with_runner_and_ufw_conf(&runner, &conf_no, &default_ports);
        assert_eq!(st_no.kind, "ufw");
        assert!(!st_no.active);
        assert_eq!(st_no.ports_allowed, None);

        let st_systemd = detect_with_runner_and_ufw_conf(&runner, &conf_missing, &default_ports);
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
    fn test_firewall_ports_from_config_security_modes_and_specs() {
        let mut cfg = Config::default();
        cfg.sharing.security = "trusted-lan".to_string();
        cfg.discovery.mdns = true;
        let ports = FirewallPorts::from_config(&cfg);
        assert_eq!(ports.legacy_audio_tcp, Some(4713));
        assert_eq!(ports.control_tcp, 4714);
        assert_eq!(ports.pair_tcp, 4715);
        assert_eq!(ports.rtp_udp, 4716);
        assert_eq!(ports.discovery_udp, 47777);
        assert!(ports.include_mdns);
        let spec_strs: Vec<String> = ports.specs().iter().map(|s| s.port_proto()).collect();
        assert_eq!(
            spec_strs,
            vec![
                "4713/tcp",
                "4714/tcp",
                "4715/tcp",
                "4716/udp",
                "47777/udp",
                "5353/udp",
            ]
        );

        // Paired/PSK security mode omits legacy PA-TCP (4713/tcp)
        for sec in ["psk", "pin", "paired"] {
            cfg.sharing.security = sec.to_string();
            cfg.discovery.mdns = false;
            let paired_ports = FirewallPorts::from_config(&cfg);
            assert_eq!(paired_ports.legacy_audio_tcp, None);
            let paired_specs: Vec<String> = paired_ports
                .specs()
                .iter()
                .map(|s| s.port_proto())
                .collect();
            assert_eq!(
                paired_specs,
                vec!["4714/tcp", "4715/tcp", "4716/udp", "47777/udp"]
            );
        }
    }

    #[test]
    fn test_build_windows_open_commands_idempotent_and_scoped() {
        let ports_no_mdns = FirewallPorts::default();
        let cmds_no_mdns = build_windows_open_commands(&ports_no_mdns.specs());
        assert_eq!(cmds_no_mdns.len(), 10);
        let expected_rules = [
            ("SoundHop Audio (4713/tcp)", "TCP", "localport=4713"),
            ("SoundHop Control (4714/tcp)", "TCP", "localport=4714"),
            ("SoundHop Pairing (4715/tcp)", "TCP", "localport=4715"),
            ("SoundHop RTP (4716/udp)", "UDP", "localport=4716"),
            ("SoundHop Discovery (47777/udp)", "UDP", "localport=47777"),
        ];
        for (idx, (rule_name, proto, localport)) in expected_rules.iter().enumerate() {
            let del = &cmds_no_mdns[idx * 2];
            let add = &cmds_no_mdns[idx * 2 + 1];
            assert!(is_netsh_delete_rule(del));
            assert_eq!(extract_netsh_rule_name(del), Some(*rule_name));
            assert!(!is_netsh_delete_rule(add));
            assert!(add.contains(&"profile=private,domain".to_string()));
            assert!(add.contains(&format!("protocol={proto}")));
            assert!(add.contains(&(*localport).to_string()));
        }

        let ports_with_mdns = FirewallPorts {
            include_mdns: true,
            ..FirewallPorts::default()
        };
        let cmds_with_mdns = build_windows_open_commands(&ports_with_mdns.specs());
        assert_eq!(cmds_with_mdns.len(), 12);
        assert!(is_netsh_delete_rule(&cmds_with_mdns[10]));
        assert_eq!(
            extract_netsh_rule_name(&cmds_with_mdns[10]),
            Some("SoundHop mDNS (5353/udp)")
        );
        assert!(cmds_with_mdns[11].contains(&"profile=private,domain".to_string()));
        assert!(cmds_with_mdns[11].contains(&"protocol=UDP".to_string()));
        assert!(cmds_with_mdns[11].contains(&format!("localport={MDNS_PORT}")));

        // Paired mode omits legacy Audio (4713/tcp)
        let paired_ports = FirewallPorts {
            legacy_audio_tcp: None,
            ..FirewallPorts::default()
        };
        let cmds_paired = build_windows_open_commands(&paired_ports.specs());
        assert_eq!(cmds_paired.len(), 8);
        assert!(cmds_paired
            .iter()
            .all(|c| extract_netsh_rule_name(c) != Some("SoundHop Audio (4713/tcp)")));
    }

    #[test]
    fn test_build_windows_close_commands_includes_mdns() {
        let ports = FirewallPorts::default();
        let cmds = build_windows_close_commands(&ports.windows_close_specs());
        assert_eq!(cmds.len(), 8);
        assert!(cmds.iter().all(|c| is_netsh_delete_rule(c)));
        let names: Vec<Option<&str>> = cmds.iter().map(|c| extract_netsh_rule_name(c)).collect();
        assert_eq!(
            names,
            vec![
                Some("SoundHop Audio (4713/tcp)"),
                Some("SoundHop Control (4714/tcp)"),
                Some("SoundHop Pairing (4715/tcp)"),
                Some("SoundHop RTP (4716/udp)"),
                Some("SoundHop Discovery (47777/udp)"),
                Some("SoundHop mDNS (5353/udp)"),
                Some("SoundHop Web (4717/tcp)"),
                Some("SoundHop WebRTC (4718/udp)"),
            ]
        );
    }

    #[test]
    fn test_build_windows_elevated_cmd_params_open_and_close() {
        let ports = FirewallPorts {
            include_mdns: true,
            ..FirewallPorts::default()
        };
        let open_cmds = build_windows_open_commands(&ports.specs());
        let open_params = build_windows_elevated_cmd_params(&open_cmds, false);
        assert!(open_params.starts_with("/C \""));
        assert!(open_params.ends_with('"'));
        assert!(open_params.contains("name=\"SoundHop Audio (4713/tcp)\""));
        assert!(open_params.contains("name=\"SoundHop Control (4714/tcp)\""));
        assert!(open_params.contains("name=\"SoundHop Pairing (4715/tcp)\""));
        assert!(open_params.contains("name=\"SoundHop RTP (4716/udp)\""));
        assert!(open_params.contains("name=\"SoundHop Discovery (47777/udp)\""));
        assert!(open_params.contains("name=\"SoundHop mDNS (5353/udp)\""));
        assert!(open_params.contains("profile=private,domain"));
        assert!(open_params.contains(" >nul 2>&1 & netsh advfirewall firewall add rule "));
        assert!(open_params.contains(") && ("));

        let close_cmds = build_windows_close_commands(&ports.windows_close_specs());
        let close_params = build_windows_elevated_cmd_params(&close_cmds, true);
        assert!(close_params.starts_with("/C \""));
        assert!(close_params.ends_with("& exit /b 0\""));
        assert!(close_params.contains("show rule name=\"SoundHop RTP (4716/udp)\""));
        assert!(close_params.contains("show rule name=\"SoundHop mDNS (5353/udp)\""));
        assert!(close_params.contains("delete rule name=\"SoundHop mDNS (5353/udp)\""));
        assert!(close_params.contains("show rule name=\"SoundHop Web (4717/tcp)\""));
        assert!(close_params.contains("delete rule name=\"SoundHop Web (4717/tcp)\""));
        assert!(close_params.contains("show rule name=\"SoundHop WebRTC (4718/udp)\""));
        assert!(close_params.contains("delete rule name=\"SoundHop WebRTC (4718/udp)\""));
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
        let ports_no_mdns = FirewallPorts::default();
        let ports_with_mdns = FirewallPorts {
            include_mdns: true,
            ..FirewallPorts::default()
        };
        let ports_paired = FirewallPorts {
            legacy_audio_tcp: None,
            ..FirewallPorts::default()
        };

        let ufw_open = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   ALLOW       Anywhere
4714/tcp                   ALLOW       Anywhere
4715/tcp                   ALLOW       Anywhere
4716/udp                   ALLOW       Anywhere
47777/udp                  ALLOW IN    Anywhere
5353/udp                   ALLOW       Anywhere
4713/tcp (v6)              ALLOW       Anywhere (v6)
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_open, &ports_no_mdns.specs()),
            Some(true)
        );
        assert_eq!(
            parse_ufw_ports_allowed(ufw_open, &ports_with_mdns.specs()),
            Some(true)
        );

        let ufw_missing_mdns = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   ALLOW       Anywhere
4714/tcp                   ALLOW       Anywhere
4715/tcp                   ALLOW       Anywhere
4716/udp                   ALLOW       Anywhere
47777/udp                  ALLOW       Anywhere
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_missing_mdns, &ports_no_mdns.specs()),
            Some(true)
        );
        assert_eq!(
            parse_ufw_ports_allowed(ufw_missing_mdns, &ports_with_mdns.specs()),
            Some(false)
        );

        // Missing RTP (4716/udp) must report false
        let ufw_missing_rtp = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   ALLOW       Anywhere
4714/tcp                   ALLOW       Anywhere
4715/tcp                   ALLOW       Anywhere
47777/udp                  ALLOW       Anywhere
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_missing_rtp, &ports_no_mdns.specs()),
            Some(false)
        );

        // Paired mode succeeds without legacy 4713/tcp
        let ufw_paired_only = "\
Status: active

To                         Action      From
--                         ------      ----
4714/tcp                   ALLOW       Anywhere
4715/tcp                   ALLOW       Anywhere
4716/udp                   ALLOW       Anywhere
47777/udp                  ALLOW       Anywhere
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_paired_only, &ports_paired.specs()),
            Some(true)
        );
        assert_eq!(
            parse_ufw_ports_allowed(ufw_paired_only, &ports_no_mdns.specs()),
            Some(false)
        );

        let ufw_denied_or_out = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp                   DENY        Anywhere
4714/tcp                   ALLOW       Anywhere
4715/tcp                   ALLOW       Anywhere
4716/udp                   ALLOW       Anywhere
47777/udp                  ALLOW OUT   Anywhere
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_denied_or_out, &ports_no_mdns.specs()),
            Some(false)
        );

        let ufw_v6_only = "\
Status: active

To                         Action      From
--                         ------      ----
4713/tcp (v6)              ALLOW       Anywhere (v6)
4714/tcp (v6)              ALLOW       Anywhere (v6)
4715/tcp (v6)              ALLOW       Anywhere (v6)
4716/udp (v6)              ALLOW       Anywhere (v6)
47777/udp (v6)             ALLOW       Anywhere (v6)
";
        assert_eq!(
            parse_ufw_ports_allowed(ufw_v6_only, &ports_no_mdns.specs()),
            Some(false)
        );

        assert_eq!(
            parse_ufw_ports_allowed("Status: inactive\n", &ports_no_mdns.specs()),
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
                if matches!(
                    query,
                    "--query-port=4713/tcp"
                        | "--query-port=4714/tcp"
                        | "--query-port=4715/tcp"
                        | "--query-port=4716/udp"
                        | "--query-port=47777/udp"
                        | "--query-service=mdns"
                ) {
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

        let with_mdns = FirewallPorts {
            include_mdns: true,
            ..FirewallPorts::default()
        };
        let st_with_mdns = detect_with_runner_for_ports(&runner, &with_mdns);
        assert_eq!(st_with_mdns.kind, "firewalld");
        assert!(st_with_mdns.active);
        assert_eq!(st_with_mdns.ports_allowed, Some(true));

        let custom_rtp = FirewallPorts {
            rtp_udp: 4799,
            ..FirewallPorts::default()
        };
        let st_missing_custom = detect_with_runner_for_ports(&runner, &custom_rtp);
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
                || joined.contains("SoundHop Control (4714/tcp)")
                || joined.contains("SoundHop Pairing (4715/tcp)")
                || joined.contains("SoundHop RTP (4716/udp)")
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
            check_windows_ports_allowed(&runner, &FirewallPorts::default().specs()),
            Some(true)
        );
        let with_mdns = FirewallPorts {
            include_mdns: true,
            ..FirewallPorts::default()
        };
        assert_eq!(
            check_windows_ports_allowed(&runner, &with_mdns.specs()),
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

        let ports_with_mdns = FirewallPorts {
            include_mdns: true,
            ..FirewallPorts::default()
        };
        let ports_no_mdns = FirewallPorts::default();

        // Non-TTY (GUI/AppImage launch): prefers single batched `pkexec sh -c ...` instead of `sudo -n`.
        let res =
            open_ports_with_runner(&runner, &ports_with_mdns, false, false, false, false, false);
        assert!(res.ok);
        assert!(res.ran);
        assert_eq!(res.firewall.ports_allowed, Some(true));
        assert_eq!(res.commands.len(), 1);
        assert_eq!(res.commands[0][0], "pkexec");
        assert_eq!(res.commands[0][1], "sh");
        assert_eq!(res.commands[0][2], "-c");
        assert_eq!(
            res.commands[0][3],
            "ufw allow 4713/tcp && ufw allow 4714/tcp && ufw allow 4715/tcp && ufw allow 4716/udp && ufw allow 47777/udp && ufw allow 5353/udp"
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
            open_ports_with_runner(&runner, &ports_no_mdns, true, true, false, true, false);
        assert_eq!(tty_res.commands.len(), 5);
        assert_eq!(
            tty_res.commands[0],
            vec!["sudo", "ufw", "allow", "4713/tcp"]
        );
        assert_eq!(
            tty_res.commands[3],
            vec!["sudo", "ufw", "allow", "4716/udp"]
        );

        // Non-TTY without `pkexec`: falls back to `sudo -n`.
        runner.available_commands.lock().unwrap().remove("pkexec");
        let fallback_res =
            open_ports_with_runner(&runner, &ports_no_mdns, true, false, false, false, false);
        assert_eq!(fallback_res.commands.len(), 5);
        assert_eq!(
            fallback_res.commands[0],
            vec!["sudo", "-n", "ufw", "allow", "4713/tcp"]
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn test_firewalld_and_nftables_multi_port_commands() {
        let fw_runner = MockRunner::new(|args, _| {
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
                stdout: "no\n".to_string(),
                stderr: String::new(),
            })
        });
        fw_runner
            .available_commands
            .lock()
            .unwrap()
            .insert("firewall-cmd".to_string());

        let paired_with_mdns = FirewallPorts {
            legacy_audio_tcp: None,
            include_mdns: true,
            ..FirewallPorts::default()
        };
        let fw_res = open_ports_with_runner(
            &fw_runner,
            &paired_with_mdns,
            true,
            false,
            true,
            false,
            false,
        );
        assert_eq!(
            fw_res.commands,
            vec![
                vec!["firewall-cmd", "--add-port=4714/tcp", "--permanent"],
                vec!["firewall-cmd", "--add-port=4715/tcp", "--permanent"],
                vec!["firewall-cmd", "--add-port=4716/udp", "--permanent"],
                vec!["firewall-cmd", "--add-port=47777/udp", "--permanent"],
                vec!["firewall-cmd", "--add-port=5353/udp", "--permanent"],
                vec!["firewall-cmd", "--add-service=mdns", "--permanent"],
                vec!["firewall-cmd", "--reload"],
            ]
        );

        let close_res = close_ports_with_runner(
            &fw_runner,
            &FirewallPorts::default(),
            true,
            false,
            true,
            false,
            false,
        );
        assert_eq!(
            close_res.commands,
            vec![
                vec!["firewall-cmd", "--remove-port=4713/tcp", "--permanent"],
                vec!["firewall-cmd", "--remove-port=4714/tcp", "--permanent"],
                vec!["firewall-cmd", "--remove-port=4715/tcp", "--permanent"],
                vec!["firewall-cmd", "--remove-port=4716/udp", "--permanent"],
                vec!["firewall-cmd", "--remove-port=47777/udp", "--permanent"],
                vec!["firewall-cmd", "--reload"],
            ]
        );

        let nft_runner = MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        });
        nft_runner
            .available_commands
            .lock()
            .unwrap()
            .insert("nft".to_string());

        let nft_res = open_ports_with_runner(
            &nft_runner,
            &paired_with_mdns,
            false,
            false,
            true,
            false,
            false,
        );
        assert!(!nft_res.note.contains("tcp dport 4713"));
        assert!(nft_res
            .note
            .contains("nft add rule inet filter input tcp dport 4714 accept"));
        assert!(nft_res
            .note
            .contains("nft add rule inet filter input tcp dport 4715 accept"));
        assert!(nft_res
            .note
            .contains("nft add rule inet filter input udp dport 4716 accept"));
        assert!(nft_res
            .note
            .contains("nft add rule inet filter input udp dport 47777 accept"));
        assert!(nft_res
            .note
            .contains("nft add rule inet filter input udp dport 5353 accept"));
    }

    #[test]
    fn test_firewall_ports_web_broadcast_from_config_specs_and_cleanup() {
        let mut cfg = Config::default();
        assert!(!cfg.sharing.web_enabled);
        let disabled = FirewallPorts::from_config(&cfg);
        assert_eq!(disabled.web_tcp, None);
        assert_eq!(disabled.webrtc_udp, None);
        assert!(disabled
            .specs()
            .iter()
            .all(|s| s.port != DEFAULT_WEB_PORT && s.port != DEFAULT_WEBRTC_PORT));

        // Ad-hoc override opens Web + WebRTC ports even when sharing.web_enabled is false.
        let adhoc = FirewallPorts::from_config_with_web(&cfg, true, 4917, 4918);
        assert_eq!(adhoc.web_tcp, Some(4917));
        assert_eq!(adhoc.webrtc_udp, Some(4918));
        let adhoc_specs: Vec<(String, String)> = adhoc
            .specs()
            .into_iter()
            .map(|s| (s.port_proto(), s.windows_rule_name()))
            .collect();
        assert!(adhoc_specs.contains(&(
            "4917/tcp".to_string(),
            "SoundHop Web (4917/tcp)".to_string()
        )));
        assert!(adhoc_specs.contains(&(
            "4918/udp".to_string(),
            "SoundHop WebRTC (4918/udp)".to_string()
        )));

        // Config-enabled Web Broadcast includes default 4717/tcp and 4718/udp.
        cfg.sharing.web_enabled = true;
        let enabled = FirewallPorts::from_config(&cfg);
        assert_eq!(enabled.web_tcp, Some(DEFAULT_WEB_PORT));
        assert_eq!(enabled.webrtc_udp, Some(DEFAULT_WEBRTC_PORT));
        let enabled_rules: Vec<String> = enabled
            .specs()
            .into_iter()
            .map(|s| s.windows_rule_name())
            .collect();
        assert!(enabled_rules.contains(&"SoundHop Web (4717/tcp)".to_string()));
        assert!(enabled_rules.contains(&"SoundHop WebRTC (4718/udp)".to_string()));

        // windows_close_specs preserves custom ports when set and populates defaults when None.
        let custom_close_rules: Vec<String> = adhoc
            .windows_close_specs()
            .into_iter()
            .map(|s| s.windows_rule_name())
            .collect();
        assert!(custom_close_rules.contains(&"SoundHop Web (4917/tcp)".to_string()));
        assert!(custom_close_rules.contains(&"SoundHop WebRTC (4918/udp)".to_string()));

        let default_close_rules: Vec<String> = FirewallPorts::default()
            .windows_close_specs()
            .into_iter()
            .map(|s| s.windows_rule_name())
            .collect();
        assert!(default_close_rules.contains(&"SoundHop Web (4717/tcp)".to_string()));
        assert!(default_close_rules.contains(&"SoundHop WebRTC (4718/udp)".to_string()));
    }
}
