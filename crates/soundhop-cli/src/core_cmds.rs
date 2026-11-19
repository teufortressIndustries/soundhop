//! Core CLI subcommand handlers (`setup`, `status`, `discover`, `connect`, `disconnect`,
//! `share`, `pair`, `apps`, `route`, `unroute`, `mic`, `volume`, `auto`, `daemon`, `stop`,
//! `service`, `firewall`, `config`, `logs`, `completions`, `version`).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use clap::CommandFactory;
use clap_complete::{generate, shells};
use soundhop_core::config::Config;
use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::daemon::Daemon;
use soundhop_core::discovery::{discover_once_with_psk, Peer};
use soundhop_core::firewall::{detect as detect_firewall, open_ports};
use soundhop_core::ipc::{
    is_reachable as ipc_is_reachable, is_running as ipc_is_running, request as ipc_request,
    IpcError,
};
use soundhop_core::security::generate_pairing_pin;
use soundhop_core::service::{
    install as install_service, restart_service, start_service, status as service_status,
    stop_service, sync_appimage_paths_if_installed, uninstall as uninstall_service,
};
use soundhop_core::state::PairedPeer;
use soundhop_core::util::{config_path, is_private_address, split_host_port};

use crate::parser::{
    AutoArgs, Cli, CompletionsArgs, ConfigArgs, ConnectArgs, DiscoverArgs, FirewallArgs, LogsArgs,
    MicArgs, PairArgs, RouteArgs, ServiceArgs, SetupArgs, ShareAction, ShareArgs, ShellChoice,
    UnrouteArgs, VolumeArgs,
};
use crate::ui::{
    emit_error, header, info, lock_or_recover, ok, print_json, safe_print, sym_arrow, warning,
};

pub(crate) fn try_ipc(
    command: &str,
    payload: serde_json::Value,
    timeout: Duration,
) -> Option<Result<serde_json::Value, String>> {
    if !ipc_is_running(None) {
        return None;
    }
    match ipc_request(command, payload, None, timeout) {
        Ok(resp) => Some(Ok(resp)),
        Err(IpcError::DaemonNotRunning) => None,
        Err(IpcError::Error(e)) => Some(Err(e)),
    }
}

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
        Some(open_ports(
            cfg.sharing.port,
            cfg.discovery.port,
            false,
            cfg.discovery.mdns,
        ))
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
            ok("Background Daemon & Tray Autostart: Installed and started");
        } else if enabled {
            warning(
                "Background Daemon & Tray Autostart: Installed and enabled, but failed to start",
            );
        } else {
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

pub fn handle_status(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let st = match try_ipc("status", serde_json::json!({}), Duration::from_secs(2)) {
        Some(Ok(resp)) => resp,
        Some(Err(e)) => {
            emit_error(&e, json_out);
            return 1;
        }
        None => controller.status(None),
    };

    if json_out {
        print_json(&st);
        return 0;
    }

    header(&format!("SoundHop v{VERSION} Status"));
    let dev_name = st
        .get("device")
        .and_then(|d| d.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let dev_id = st
        .get("device")
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    info(&format!("Device: {dev_name} ({dev_id})"));

    let sharing_on = st
        .get("sharing")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sharing_port = st
        .get("sharing")
        .and_then(|s| s.get("port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(4713);
    let sec_mode = st
        .get("sharing")
        .and_then(|s| s.get("security"))
        .and_then(|v| v.as_str())
        .unwrap_or("trusted-lan");

    if sharing_on {
        ok(&format!(
            "Speaker Sharing: Active on TCP :{sharing_port} (security: {sec_mode})"
        ));
    } else {
        info("Speaker Sharing: Disabled");
    }

    if let Some(conn) = st.get("connection").filter(|c| !c.is_null()) {
        let name = conn.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let host = conn.get("host").and_then(|v| v.as_str()).unwrap_or("");
        let port = conn.get("port").and_then(|v| v.as_u64()).unwrap_or(4713);
        let multi = conn
            .get("multiroom")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let dur = conn.get("duration").and_then(|v| v.as_str()).unwrap_or("");
        let rtt = conn
            .get("rtt_ms")
            .and_then(|v| v.as_f64())
            .map(|r| format!(", RTT {r:.1} ms"))
            .unwrap_or_default();
        if multi {
            ok(&format!("Multi-Room Broadcast: {name} (uptime {dur}{rtt})"));
        } else {
            ok(&format!(
                "Connected Speaker: {name} [{host}:{port}] (uptime {dur}{rtt})"
            ));
        }
    } else {
        info("Connection: Not connected (playing on local output)");
    }

    if let Some(vol) = st.get("volume").filter(|v| !v.is_null()) {
        let lvl = vol.get("level").and_then(|v| v.as_u64()).unwrap_or(100);
        let mute = vol.get("mute").and_then(|v| v.as_bool()).unwrap_or(false);
        info(&format!(
            "Volume: {lvl}%{}",
            if mute { " (MUTED)" } else { "" }
        ));
    }

    0
}

pub fn handle_discover(controller: &Arc<Controller>, args: &DiscoverArgs, json_out: bool) -> i32 {
    let timeout = if args.timeout.is_finite() {
        args.timeout.max(0.3)
    } else {
        2.5
    };
    let ipc_timeout = Duration::from_secs_f64((timeout + 3.0).max(5.0));
    let peers: Vec<Peer> = match try_ipc(
        "discover",
        serde_json::json!({ "timeout": timeout }),
        ipc_timeout,
    ) {
        Some(Ok(resp)) => resp
            .get("peers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| serde_json::from_value::<Peer>(item.clone()).ok())
                    .collect()
            })
            .unwrap_or_default(),
        Some(Err(e)) => {
            emit_error(&e, json_out);
            return 1;
        }
        None => {
            let cfg = lock_or_recover(&controller.config).clone();
            let psk = (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
            let found = discover_once_with_psk(timeout, cfg.discovery.port, &cfg.device.id, psk);
            for p in &found {
                controller.remember_peer(p);
            }
            controller.save();
            found
        }
    };

    if json_out {
        print_json(&peers);
        return 0;
    }

    header("Discovered SoundHop Speakers on LAN");
    if peers.is_empty() {
        info("No peers discovered yet. Ensure remote devices have `soundhop share on` active.");
    } else {
        for p in &peers {
            let rtt = p
                .rtt_ms
                .map(|r| format!("  RTT: {r:.1} ms"))
                .unwrap_or_default();
            ok(&format!(
                "{:<22} {}:{}  [id: {}, platform: {}, sec: {}]{}",
                p.name, p.host, p.port, p.id, p.platform, p.security, rtt
            ));
        }
    }
    0
}

fn resolve_cli_peer(controller: &Arc<Controller>, target: &str) -> Result<Peer, String> {
    let q = target.trim();
    if q.is_empty() {
        return Err("Specify a non-empty peer name, ID, or IP.".to_string());
    }
    let cfg = lock_or_recover(&controller.config).clone();
    if let Some(kp) = controller.known_peer(q) {
        return Ok(kp);
    }
    let st = lock_or_recover(&controller.state).clone();
    let mut known: Vec<_> = st.known_peers.values().cloned().collect();
    known.sort_by(|a, b| a.id.cmp(&b.id));
    let q_lower = q.to_lowercase();
    for kp in &known {
        if kp.name.eq_ignore_ascii_case(q) || kp.host.eq_ignore_ascii_case(q) {
            if let Some(p) = controller.known_peer(&kp.id) {
                return Ok(p);
            }
        }
    }
    for kp in &known {
        if kp.name.to_lowercase().contains(&q_lower) {
            if let Some(p) = controller.known_peer(&kp.id) {
                return Ok(p);
            }
        }
    }
    let (host, port) = split_host_port(q, cfg.sharing.port);
    if is_private_address(&host) {
        return Ok(Peer {
            id: format!("manual-{host}-{port}"),
            name: host.clone(),
            host,
            port,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
            last_seen: soundhop_core::util::now_secs(),
            source: "manual".to_string(),
            rtt_ms: None,
        });
    }
    // Quick scan fallback
    let psk = (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
    let mut scanned = discover_once_with_psk(0.9, cfg.discovery.port, &cfg.device.id, psk);
    scanned.sort_by(|a, b| a.id.cmp(&b.id));
    for p in &scanned {
        controller.remember_peer(p);
    }
    if let Some(p) = scanned.iter().find(|p| {
        p.id.eq_ignore_ascii_case(q)
            || p.name.eq_ignore_ascii_case(q)
            || p.host.eq_ignore_ascii_case(q)
    }) {
        return Ok(p.clone());
    }
    if let Some(p) = scanned
        .iter()
        .find(|p| p.name.to_lowercase().contains(&q_lower))
    {
        return Ok(p.clone());
    }
    Err(format!("Speaker '{q}' not found on LAN."))
}

pub fn handle_connect(controller: &Arc<Controller>, args: &ConnectArgs, json_out: bool) -> i32 {
    if let Some(lat) = args.latency {
        lock_or_recover(&controller.config).connection.latency_ms = Some(lat);
    }
    let switch_default = if args.default_sink {
        Some(true)
    } else if args.no_default {
        Some(false)
    } else {
        None
    };

    let mut payload = if args.targets.len() > 1 || args.combine {
        serde_json::json!({
            "peers": args.targets,
            "keep_local": args.keep_local,
            "switch_default": switch_default,
            "remember": args.remember,
        })
    } else {
        serde_json::json!({
            "peer": args.targets.first().cloned().unwrap_or_default(),
            "keep_local": args.keep_local,
            "switch_default": switch_default,
            "remember": args.remember,
        })
    };
    if let Some(lat) = args.latency {
        payload["latency_ms"] = serde_json::json!(lat);
    }
    if let Some(ipc_res) = try_ipc("connect", payload, Duration::from_secs(12)) {
        return match ipc_res {
            Ok(resp) => {
                if json_out {
                    print_json(&resp);
                } else {
                    let cname = resp
                        .get("connection")
                        .and_then(|c| c.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("remote speaker");
                    ok(&format!("Connected audio output {} {cname}", sym_arrow()));
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        };
    }

    let mut peers = Vec::new();
    if args.targets.is_empty() {
        let last = lock_or_recover(&controller.config)
            .connection
            .last_peer
            .clone();
        if !last.is_empty() {
            if let Ok(p) = resolve_cli_peer(controller, &last) {
                peers.push(p);
            }
        }
        if peers.is_empty() {
            emit_error("Specify a peer name, ID, or IP to connect to.", json_out);
            return 1;
        }
    } else {
        for t in &args.targets {
            match resolve_cli_peer(controller, t) {
                Ok(p) => peers.push(p),
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            }
        }
    }

    let opts = ConnectOptions {
        switch_default,
        keep_local: args.keep_local,
        by: "cli".to_string(),
        notify_user: Some(!json_out),
        ..Default::default()
    };

    let res = if peers.len() > 1 || args.combine || args.keep_local {
        controller.connect_multi(&peers, opts)
    } else {
        controller.connect(&peers[0], opts)
    };

    match res {
        Ok(val) => {
            if args.remember {
                lock_or_recover(&controller.config).connection.last_peer = peers[0].id.clone();
                controller.save();
            }
            if json_out {
                print_json(&val);
            } else {
                let name = val.get("name").and_then(|v| v.as_str()).unwrap_or("");
                ok(&format!("Connected audio output {} {name}", sym_arrow()));
            }
            0
        }
        Err(e) => {
            emit_error(&format!("{e}"), json_out);
            1
        }
    }
}

pub fn handle_disconnect(controller: &Arc<Controller>, json_out: bool) -> i32 {
    if let Some(ipc_res) = try_ipc("disconnect", serde_json::json!({}), Duration::from_secs(4)) {
        return match ipc_res {
            Ok(resp) => {
                if json_out {
                    print_json(&resp);
                } else {
                    ok("Disconnected from remote speaker(s).");
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        };
    }
    match controller.disconnect(false, None, true, Some(!json_out), false, false) {
        Ok(val) => {
            if json_out {
                print_json(&val);
            } else {
                ok("Disconnected from remote speaker(s).");
            }
            0
        }
        Err(e) => {
            emit_error(&format!("{e}"), json_out);
            1
        }
    }
}

pub fn handle_share(controller: &Arc<Controller>, args: &ShareArgs, json_out: bool) -> i32 {
    let mut port_override = args.port;
    let enable_opt: Option<bool> = match &args.action {
        Some(ShareAction::On { port }) => {
            if let Some(p) = port {
                port_override = Some(*p);
            }
            Some(true)
        }
        Some(ShareAction::Off) => Some(false),
        Some(ShareAction::Status) | None => None,
    };
    if let Some(p) = port_override {
        lock_or_recover(&controller.config).sharing.port = p;
    }

    if let Some(enable) = enable_opt {
        let mut payload = serde_json::json!({ "enabled": enable });
        if let Some(p) = port_override {
            payload["port"] = serde_json::json!(p);
        }
        if let Some(ipc_res) = try_ipc("share", payload, Duration::from_secs(4)) {
            return match ipc_res {
                Ok(resp) => {
                    if json_out {
                        print_json(&resp);
                    } else if enable {
                        ok("Speaker sharing enabled.");
                    } else {
                        ok("Speaker sharing disabled.");
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            };
        }
        match controller.share(enable, Some(!json_out)) {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else if enable {
                    let port = val.get("port").and_then(|v| v.as_u64()).unwrap_or(4713);
                    ok(&format!("Sharing speakers on TCP port {port}."));
                } else {
                    ok("Stopped sharing speakers.");
                }
                0
            }
            Err(e) => {
                emit_error(&format!("{e}"), json_out);
                1
            }
        }
    } else {
        if let Some(ipc_res) = try_ipc("status", serde_json::json!({}), Duration::from_secs(2)) {
            match ipc_res {
                Ok(st_resp) => {
                    let sharing = st_resp
                        .get("sharing")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    if json_out {
                        print_json(&sharing);
                    } else {
                        let enabled = sharing
                            .get("enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let port = sharing.get("port").and_then(|v| v.as_u64()).unwrap_or(4713);
                        let security = sharing
                            .get("security")
                            .and_then(|v| v.as_str())
                            .unwrap_or("trusted-lan");
                        info(&format!(
                            "Sharing: {} (port {port}, security {security})",
                            if enabled { "enabled" } else { "disabled" }
                        ));
                    }
                    return 0;
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            }
        }
        let cfg = lock_or_recover(&controller.config).clone();
        let st = lock_or_recover(&controller.state).clone();
        let out = serde_json::json!({
            "enabled": cfg.sharing.enabled,
            "port": cfg.sharing.port,
            "security": cfg.sharing.security,
            "listener_module": st.listener_module,
        });
        if json_out {
            print_json(&out);
        } else {
            info(&format!(
                "Sharing: {} (port {}, security {})",
                if cfg.sharing.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                cfg.sharing.port,
                cfg.sharing.security
            ));
        }
        0
    }
}

fn pair_status_json(controller: &Controller) -> serde_json::Value {
    let st = lock_or_recover(&controller.state).clone();
    let cfg = lock_or_recover(&controller.config).clone();
    serde_json::json!({
        "security": cfg.sharing.security,
        "psk_active": !cfg.sharing.psk_hash.is_empty(),
        "paired_peers": st
            .paired_peers
            .values()
            .map(PairedPeer::to_public_json)
            .collect::<Vec<_>>(),
    })
}

pub fn handle_pair(controller: &Arc<Controller>, args: &PairArgs, json_out: bool) -> i32 {
    let target = args.target.as_deref().unwrap_or("status").trim();
    if target.eq_ignore_ascii_case("generate") {
        let pin = generate_pairing_pin();
        if json_out {
            print_json(&serde_json::json!({ "pin": pin }));
        } else {
            ok(&format!("Generated 12-character pairing code: {pin}"));
        }
        return 0;
    }
    if target.eq_ignore_ascii_case("clear") {
        if let Some(ipc_res) = try_ipc(
            "pair",
            serde_json::json!({ "clear": true }),
            Duration::from_secs(4),
        ) {
            return match ipc_res {
                Ok(resp) => {
                    if json_out {
                        print_json(&resp);
                    } else {
                        ok("Cleared PIN/PSK pairing and reverted to trusted-lan mode.");
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            };
        }
        {
            let mut cfg = lock_or_recover(&controller.config);
            cfg.sharing.psk_hash.clear();
            cfg.sharing.security = "trusted-lan".to_string();
        }
        lock_or_recover(&controller.state).paired_peers.clear();
        controller.save();
        if json_out {
            print_json(&serde_json::json!({ "cleared": true }));
        } else {
            ok("Cleared PIN/PSK pairing and reverted to trusted-lan mode.");
        }
        return 0;
    }
    if target.eq_ignore_ascii_case("status") && args.pin.is_none() {
        let out = pair_status_json(controller);
        if json_out {
            print_json(&out);
        } else {
            let security = out
                .get("security")
                .and_then(|v| v.as_str())
                .unwrap_or("trusted-lan");
            let count = out
                .get("paired_peers")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            header("SoundHop PIN / PSK Pairing Status");
            info(&format!("Security Mode: {security}"));
            info(&format!("Paired Peers: {count}"));
        }
        return 0;
    }

    if let Some(ipc_res) = try_ipc(
        "pair",
        serde_json::json!({
            "peer": target,
            "secret": args.pin,
        }),
        Duration::from_secs(6),
    ) {
        return match ipc_res {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    let pin = val.get("pin_or_psk").and_then(|v| v.as_str()).unwrap_or("");
                    let fp = val
                        .get("shared_key_fingerprint")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    ok(&format!(
                        "Paired with '{target}' using PIN/PSK '{pin}' (key fingerprint: {fp})"
                    ));
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        };
    }

    match controller.pair_peer(target, args.pin.as_deref()) {
        Ok(val) => {
            if json_out {
                print_json(&val);
            } else {
                let pin = val.get("pin_or_psk").and_then(|v| v.as_str()).unwrap_or("");
                let fp = val
                    .get("shared_key_fingerprint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                ok(&format!(
                    "Paired with '{target}' using PIN/PSK '{pin}' (key fingerprint: {fp})"
                ));
            }
            0
        }
        Err(e) => {
            emit_error(&format!("{e}"), json_out);
            1
        }
    }
}

pub fn handle_apps(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let apps = match try_ipc("apps", serde_json::json!({}), Duration::from_secs(3)) {
        Some(Ok(resp)) => resp
            .get("apps")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        Some(Err(e)) => {
            emit_error(&e, json_out);
            return 1;
        }
        None => controller.list_apps(),
    };
    if json_out {
        print_json(&apps);
        return 0;
    }
    header("Active Audio Applications & Persistent Routing Rules");
    if apps.is_empty() {
        info("No active audio streams or saved rules.");
    } else {
        for a in &apps {
            let name = a.get("app_name").and_then(|v| v.as_str()).unwrap_or("");
            let media = a.get("media_name").and_then(|v| v.as_str()).unwrap_or("");
            let rule = a
                .get("routed_rule")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            let pers = a
                .get("persistent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ok(&format!(
                "{:<20}  rule={:<8}  persistent={:<5}  media={media}",
                name,
                if rule.is_empty() { "default" } else { rule },
                pers
            ));
        }
    }
    0
}

pub fn handle_route(controller: &Arc<Controller>, args: &RouteArgs, json_out: bool) -> i32 {
    let dest = if args.local { "local" } else { "remote" };
    if let Some(ipc_res) = try_ipc(
        "route",
        serde_json::json!({
            "app": args.app,
            "target": dest,
            "save": args.save,
        }),
        Duration::from_secs(4),
    ) {
        return match ipc_res {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok(&format!(
                        "Routed '{}' {} {dest} output{}",
                        args.app,
                        sym_arrow(),
                        if args.save {
                            " (saved to config.toml)"
                        } else {
                            ""
                        }
                    ));
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        };
    }
    match controller.route_app(&args.app, Some(dest), args.save) {
        Ok(val) => {
            if json_out {
                print_json(&val);
            } else {
                ok(&format!(
                    "Routed '{}' {} {dest} output{}",
                    args.app,
                    sym_arrow(),
                    if args.save {
                        " (saved to config.toml)"
                    } else {
                        ""
                    }
                ));
            }
            0
        }
        Err(e) => {
            emit_error(&format!("{e}"), json_out);
            1
        }
    }
}

pub fn handle_unroute(controller: &Arc<Controller>, args: &UnrouteArgs, json_out: bool) -> i32 {
    if args.all || args.app.is_none() {
        if let Some(ipc_res) = try_ipc(
            "unroute",
            serde_json::json!({
                "all": true,
                "remove_rule": args.remove_rule,
            }),
            Duration::from_secs(4),
        ) {
            return match ipc_res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        ok("Cleared per-app routing overrides.");
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            };
        }
        return match controller.unroute_app(None, args.remove_rule) {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok("Cleared per-app routing overrides.");
                }
                0
            }
            Err(e) => {
                emit_error(&format!("{e}"), json_out);
                1
            }
        };
    }

    let app = args.app.as_deref().unwrap_or("");
    if let Some(ipc_res) = try_ipc(
        "unroute",
        serde_json::json!({
            "app": app,
            "all": false,
            "remove_rule": args.remove_rule,
        }),
        Duration::from_secs(4),
    ) {
        return match ipc_res {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok(&format!("Restored '{app}' to local output."));
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        };
    }
    match controller.unroute_app(Some(app), args.remove_rule) {
        Ok(val) => {
            if json_out {
                print_json(&val);
            } else {
                ok(&format!("Restored '{app}' to local output."));
            }
            0
        }
        Err(e) => {
            emit_error(&format!("{e}"), json_out);
            1
        }
    }
}

pub fn handle_mic(controller: &Arc<Controller>, args: &MicArgs, json_out: bool) -> i32 {
    let mode = args.mode.trim().to_lowercase();
    let ipc_action = match mode.as_str() {
        "send" | "tx" | "on" => Some(("send", "Sending local microphone to remote peer.")),
        "receive" | "rx" => Some(("receive", "Receiving microphone from remote peer.")),
        "off" | "stop" | "disable" => Some(("off", "Microphone pass-through disabled.")),
        "status" => None,
        other => {
            emit_error(
                &format!("Unknown mic mode '{other}'. Expected: send, receive, off, status."),
                json_out,
            );
            return 1;
        }
    };

    if let Some((act, ok_msg)) = ipc_action {
        if let Some(ipc_res) = try_ipc(
            "mic",
            serde_json::json!({ "action": act }),
            Duration::from_secs(5),
        ) {
            return match ipc_res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        ok(ok_msg);
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            };
        }
        let local_res = match act {
            "send" => controller.enable_mic("send"),
            "receive" => controller.enable_mic("receive"),
            _ => controller.disable_mic(),
        };
        match local_res {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok(ok_msg);
                }
                0
            }
            Err(e) => {
                emit_error(&format!("{e}"), json_out);
                1
            }
        }
    } else {
        let out = match try_ipc(
            "mic",
            serde_json::json!({ "action": "status" }),
            Duration::from_secs(2),
        ) {
            Some(Ok(resp)) => resp,
            Some(Err(e)) => {
                emit_error(&e, json_out);
                return 1;
            }
            None => {
                let st = lock_or_recover(&controller.state).clone();
                serde_json::json!({
                    "enabled": !st.connected_source.is_empty(),
                    "direction": st.mic_direction,
                    "source": st.connected_source,
                })
            }
        };
        if json_out {
            print_json(&out);
        } else {
            let enabled = out
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let dir = out.get("direction").and_then(|v| v.as_str()).unwrap_or("");
            info(&format!(
                "Microphone Pass-Through: {} ({})",
                if enabled { "active" } else { "off" },
                if dir.is_empty() { "none" } else { dir }
            ));
        }
        0
    }
}

pub fn parse_volume_input(raw: &str, cur_vol: i32) -> Result<u32, String> {
    let val_str = raw.trim().to_lowercase();
    let clean = val_str.strip_suffix('%').unwrap_or(&val_str);
    if clean.is_empty() {
        return Err(
            "Specify a volume level (`0-150`, `+10`, `-10`, `mute`, `unmute`, `toggle`)."
                .to_string(),
        );
    }
    let target = if let Some(delta_str) = clean.strip_prefix('+') {
        if delta_str.is_empty() || !delta_str.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("Invalid volume delta '{raw}'."));
        }
        let delta = delta_str
            .parse::<i32>()
            .map_err(|_| format!("Volume delta '{raw}' is out of range."))?;
        cur_vol.saturating_add(delta)
    } else if let Some(delta_str) = clean.strip_prefix('-') {
        if delta_str.is_empty() || !delta_str.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("Invalid volume delta '{raw}'."));
        }
        let delta = delta_str
            .parse::<i32>()
            .map_err(|_| format!("Volume delta '{raw}' is out of range."))?;
        cur_vol.saturating_sub(delta)
    } else {
        if !clean.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!(
                "Invalid volume '{raw}'. Expected `0-150`, `+10`, `-10`, `mute`, `unmute`, or `toggle`."
            ));
        }
        clean
            .parse::<i32>()
            .map_err(|_| format!("Volume '{raw}' is out of range."))?
    };
    Ok(target.clamp(0, 150) as u32)
}

pub fn handle_volume(controller: &Arc<Controller>, args: &VolumeArgs, json_out: bool) -> i32 {
    let get_cur = || -> Result<serde_json::Value, String> {
        match try_ipc("volume", serde_json::json!({}), Duration::from_secs(2)) {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(e),
            None => Ok(controller.get_volume()),
        }
    };

    let Some(ref raw) = args.value else {
        let v = match get_cur() {
            Ok(v) => v,
            Err(e) => {
                emit_error(&e, json_out);
                return 1;
            }
        };
        if json_out {
            print_json(&v);
        } else {
            let lvl = v.get("volume").and_then(|x| x.as_u64()).unwrap_or(100);
            let mute = v.get("mute").and_then(|x| x.as_bool()).unwrap_or(false);
            info(&format!(
                "Volume: {lvl}%{}",
                if mute { " (muted)" } else { "" }
            ));
        }
        return 0;
    };

    let val_str = raw.trim().to_lowercase();
    let (req_vol, req_mute): (Option<u32>, Option<bool>) = match val_str.as_str() {
        "mute" => (None, Some(true)),
        "unmute" => (None, Some(false)),
        "toggle" => {
            let cur = match get_cur() {
                Ok(v) => v.get("mute").and_then(|x| x.as_bool()).unwrap_or(false),
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            };
            (None, Some(!cur))
        }
        _ => {
            let cur_vol = match get_cur() {
                Ok(v) => v.get("volume").and_then(|x| x.as_u64()).unwrap_or(100) as i32,
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            };
            match parse_volume_input(raw, cur_vol) {
                Ok(v) => (Some(v), None),
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            }
        }
    };

    let res = match try_ipc(
        "volume",
        serde_json::json!({ "volume": req_vol, "mute": req_mute }),
        Duration::from_secs(3),
    ) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            emit_error(&e, json_out);
            return 1;
        }
        None => controller.set_volume(req_vol, req_mute, true),
    };

    if json_out {
        print_json(&res);
    } else {
        let lvl = res.get("volume").and_then(|x| x.as_u64()).unwrap_or(100);
        let mute = res.get("mute").and_then(|x| x.as_bool()).unwrap_or(false);
        ok(&format!(
            "Volume set to {lvl}%{}",
            if mute { " (muted)" } else { "" }
        ));
    }
    0
}

pub fn handle_auto(controller: &Arc<Controller>, args: &AutoArgs, json_out: bool) -> i32 {
    let act = args.action.trim().to_lowercase();
    match act.as_str() {
        "on" | "enable" => {
            let v = match try_ipc(
                "auto",
                serde_json::json!({ "action": "on" }),
                Duration::from_secs(2),
            ) {
                Some(Ok(resp)) => resp,
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    return 1;
                }
                None => controller.set_auto(true),
            };
            if json_out {
                print_json(&v);
            } else {
                ok("Auto-connect enabled.");
            }
            0
        }
        "off" | "disable" => {
            let v = match try_ipc(
                "auto",
                serde_json::json!({ "action": "off" }),
                Duration::from_secs(2),
            ) {
                Some(Ok(resp)) => resp,
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    return 1;
                }
                None => controller.set_auto(false),
            };
            if json_out {
                print_json(&v);
            } else {
                ok("Auto-connect disabled.");
            }
            0
        }
        "prefer" => {
            if let Some(ref p) = args.peer {
                let peer_trimmed = p.trim();
                if peer_trimmed.is_empty() {
                    emit_error("Specify a non-empty peer ID or name to prefer.", json_out);
                    return 1;
                }
                if let Some(ipc_res) = try_ipc(
                    "config_patch",
                    serde_json::json!({
                        "patch": { "connection": { "last_peer": peer_trimmed } }
                    }),
                    Duration::from_secs(2),
                ) {
                    if let Err(e) = ipc_res {
                        emit_error(&e, json_out);
                        return 1;
                    }
                } else {
                    lock_or_recover(&controller.config).connection.last_peer =
                        peer_trimmed.to_string();
                    controller.save();
                }
                if json_out {
                    print_json(&serde_json::json!({ "preferred_peer": peer_trimmed }));
                } else {
                    ok(&format!(
                        "Preferred auto-connect peer set to '{peer_trimmed}'."
                    ));
                }
                0
            } else {
                emit_error("Specify a peer ID or name to prefer.", json_out);
                1
            }
        }
        "status" => {
            let out = match try_ipc(
                "auto",
                serde_json::json!({ "action": "status" }),
                Duration::from_secs(2),
            ) {
                Some(Ok(mut resp)) => {
                    if resp.get("preferred_peer").is_none() {
                        let last = lock_or_recover(&controller.config)
                            .connection
                            .last_peer
                            .clone();
                        if let Some(obj) = resp.as_object_mut() {
                            obj.insert("preferred_peer".to_string(), serde_json::json!(last));
                        }
                    }
                    resp
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    return 1;
                }
                None => {
                    let cfg = lock_or_recover(&controller.config).clone();
                    let st = lock_or_recover(&controller.state).clone();
                    serde_json::json!({
                        "auto_connect": cfg.connection.auto_connect,
                        "paused": st.auto_paused,
                        "preferred_peer": cfg.connection.last_peer,
                    })
                }
            };
            if json_out {
                print_json(&out);
            } else {
                let auto_on = out
                    .get("auto_connect")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let paused = out.get("paused").and_then(|v| v.as_bool()).unwrap_or(false);
                let pref = out
                    .get("preferred_peer")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                info(&format!(
                    "Auto-Connect: {auto_on} (paused: {paused}, preferred: '{pref}')"
                ));
            }
            0
        }
        other => {
            emit_error(
                &format!("Unknown auto action '{other}'. Expected: on, off, status, prefer."),
                json_out,
            );
            1
        }
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
    let mut stopped_daemon = false;
    if ipc_is_reachable(None)
        && ipc_request("stop", serde_json::json!({}), None, Duration::from_secs(3)).is_ok()
    {
        stopped_daemon = true;
    }
    let _ = controller.disconnect(false, None, true, Some(false), true, true);
    if json_out {
        print_json(&serde_json::json!({
            "stopped_daemon": stopped_daemon,
            "disconnected": true,
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
                    ok("Installed, enabled, and started SoundHop user service.");
                    if let Some(tray_path) = res
                        .get("tray_autostart")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        info(&format!("Tray autostart: {tray_path}"));
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
                    ok("Removed SoundHop user service and autostart entries.");
                } else {
                    info("SoundHop user service was not installed.");
                }
            }
            "start" => {
                if action_ok {
                    ok("Started SoundHop user service.");
                } else {
                    emit_error("Failed to start SoundHop user service.", false);
                }
            }
            "stop" => {
                if action_ok {
                    ok("Stopped SoundHop user service.");
                } else {
                    emit_error("Failed to stop SoundHop user service.", false);
                }
            }
            "restart" => {
                if action_ok {
                    ok("Restarted SoundHop user service.");
                } else {
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
                let unit = res.get("unit").and_then(|v| v.as_str()).unwrap_or("");
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
            let res = open_ports(
                cfg.sharing.port,
                cfg.discovery.port,
                false,
                cfg.discovery.mdns,
            );
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
            let st = detect_firewall();
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

fn config_json_diff(
    before: &serde_json::Value,
    after: &serde_json::Value,
    dotted_key: &str,
) -> serde_json::Value {
    let mut patch = serde_json::Map::new();
    if let (Some(b_obj), Some(a_obj)) = (before.as_object(), after.as_object()) {
        for (sec_name, a_sec) in a_obj {
            if let (Some(b_sec_obj), Some(a_sec_obj)) = (
                b_obj.get(sec_name).and_then(|v| v.as_object()),
                a_sec.as_object(),
            ) {
                let mut sec_diff = serde_json::Map::new();
                for (k, v) in a_sec_obj {
                    if b_sec_obj.get(k) != Some(v) {
                        sec_diff.insert(k.clone(), v.clone());
                    }
                }
                if !sec_diff.is_empty() {
                    patch.insert(sec_name.clone(), serde_json::Value::Object(sec_diff));
                }
            }
        }
        if patch.is_empty() {
            let parts: Vec<&str> = dotted_key.split('.').collect();
            if parts.len() == 2 {
                if let Some(val) = a_obj.get(parts[0]).and_then(|s| s.get(parts[1])) {
                    let mut sec = serde_json::Map::new();
                    sec.insert(parts[1].to_string(), val.clone());
                    patch.insert(parts[0].to_string(), serde_json::Value::Object(sec));
                }
            }
        }
    }
    serde_json::Value::Object(patch)
}

pub fn handle_config(controller: &Arc<Controller>, args: &ConfigArgs, json_out: bool) -> i32 {
    let act = args.action.trim().to_lowercase();
    match act.as_str() {
        "path" => {
            let p = config_path();
            if json_out {
                print_json(&serde_json::json!({ "path": p.to_string_lossy() }));
            } else {
                safe_print(&p.to_string_lossy());
            }
            0
        }
        "reset" => {
            let mut def = Config::default();
            if let Some(ipc_res) = try_ipc(
                "config_patch",
                serde_json::json!({ "patch": def.to_json_value() }),
                Duration::from_secs(4),
            ) {
                return match ipc_res {
                    Ok(resp) => {
                        if let Some(cfg_val) = resp.get("config") {
                            if let Ok(parsed) = serde_json::from_value::<Config>(cfg_val.clone()) {
                                *lock_or_recover(&controller.config) = parsed;
                            }
                        }
                        if json_out {
                            print_json(resp.get("config").unwrap_or(&def.to_json_value()));
                        } else {
                            ok("Reset configuration to defaults.");
                        }
                        0
                    }
                    Err(e) => {
                        emit_error(&e, json_out);
                        1
                    }
                };
            }
            if let Err(e) = def.save(None) {
                emit_error(
                    &format!("Failed to save reset configuration: {e}"),
                    json_out,
                );
                return 1;
            }
            *lock_or_recover(&controller.config) = def.clone();
            if json_out {
                print_json(&def.to_json_value());
            } else {
                ok("Reset configuration to defaults.");
            }
            0
        }
        "get" => {
            let Some(ref key) = args.key else {
                emit_error(
                    "Specify a dotted key to read (e.g. `device.name`).",
                    json_out,
                );
                return 1;
            };
            let cfg = lock_or_recover(&controller.config).clone();
            match cfg.get_dotted(key) {
                Ok(val) => {
                    if json_out {
                        print_json(&serde_json::json!({ "key": key, "value": val }));
                    } else if let Some(s) = val.as_str() {
                        safe_print(s);
                    } else {
                        safe_print(&val.to_string());
                    }
                    0
                }
                Err(e) => {
                    emit_error(&format!("{e}"), json_out);
                    1
                }
            }
        }
        "set" => {
            let (Some(ref key), Some(ref val)) = (&args.key, &args.value) else {
                emit_error(
                    "Usage: `soundhop config set <section.key> <value>`",
                    json_out,
                );
                return 1;
            };
            let mut cfg = lock_or_recover(&controller.config).clone();
            let before_json = cfg.to_json_value();
            match cfg.set_dotted(key, val) {
                Ok(()) => {
                    let after_json = cfg.to_json_value();
                    let patch = config_json_diff(&before_json, &after_json, key);
                    if let Some(ipc_res) = try_ipc(
                        "config_patch",
                        serde_json::json!({ "patch": patch }),
                        Duration::from_secs(4),
                    ) {
                        return match ipc_res {
                            Ok(_) => {
                                *lock_or_recover(&controller.config) = cfg;
                                if json_out {
                                    print_json(
                                        &serde_json::json!({ "ok": true, "key": key, "value": val }),
                                    );
                                } else {
                                    ok(&format!("Updated {key} = {val}"));
                                }
                                0
                            }
                            Err(e) => {
                                emit_error(&e, json_out);
                                1
                            }
                        };
                    }
                    if let Err(e) = cfg.save(None) {
                        emit_error(&format!("Failed to save configuration: {e}"), json_out);
                        return 1;
                    }
                    *lock_or_recover(&controller.config) = cfg;
                    if json_out {
                        print_json(&serde_json::json!({ "ok": true, "key": key, "value": val }));
                    } else {
                        ok(&format!("Updated {key} = {val}"));
                    }
                    0
                }
                Err(e) => {
                    emit_error(&format!("{e}"), json_out);
                    1
                }
            }
        }
        "show" => {
            let cfg = lock_or_recover(&controller.config).clone();
            if json_out {
                print_json(&cfg.to_json_value());
            } else {
                safe_print(&cfg.to_toml_string());
            }
            0
        }
        other => {
            emit_error(
                &format!("Unknown config action '{other}'. Expected: show, path, get, set, reset."),
                json_out,
            );
            1
        }
    }
}

pub fn handle_logs(controller: &Arc<Controller>, args: &LogsArgs, json_out: bool) -> i32 {
    if let Some(ipc_res) = try_ipc(
        "logs",
        serde_json::json!({ "lines": args.lines }),
        Duration::from_secs(4),
    ) {
        return match ipc_res {
            Ok(resp) => {
                let all_lines: Vec<String> = resp
                    .get("lines")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_str().map(ToString::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let start = all_lines.len().saturating_sub(args.lines);
                let tail = &all_lines[start..];
                if json_out {
                    print_json(&serde_json::json!({
                        "ok": true,
                        "lines": args.lines,
                        "logs": tail,
                    }));
                } else if tail.is_empty() {
                    info("No recent daemon/session logs recorded.");
                } else {
                    for line in tail {
                        safe_print(line);
                    }
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        };
    }

    let sess = controller.android_session.status();
    let logs = sess
        .get("recent_logs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if json_out {
        print_json(&serde_json::json!({
            "ok": true,
            "daemon_running": false,
            "lines": args.lines,
            "logs": logs,
        }));
    } else if logs.is_empty() {
        info("SoundHop daemon is not running; no recent session logs available.");
    } else {
        for l in logs.iter().rev().take(args.lines).rev() {
            if let Some(s) = l.as_str() {
                safe_print(s);
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::state::{KnownPeer, State};
    use soundhop_core::util::{CommandResult, MockRunner};

    fn test_controller() -> Arc<Controller> {
        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Arc::new(Controller::new(
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        ));
        ctrl.set_save_enabled(false);
        ctrl
    }

    #[test]
    fn test_parse_volume_input_strict() {
        assert_eq!(parse_volume_input("80", 50).unwrap(), 80);
        assert_eq!(parse_volume_input("120%", 50).unwrap(), 120);
        assert_eq!(parse_volume_input("200", 50).unwrap(), 150);
        assert_eq!(parse_volume_input("+15", 50).unwrap(), 65);
        assert_eq!(parse_volume_input("-20%", 50).unwrap(), 30);
        assert_eq!(parse_volume_input("-80", 50).unwrap(), 0);
        assert!(parse_volume_input("abc", 50).is_err());
        assert!(parse_volume_input("+abc", 50).is_err());
        assert!(parse_volume_input("+", 50).is_err());
        assert!(parse_volume_input("", 50).is_err());
    }

    #[test]
    fn test_resolve_cli_peer_empty_and_deterministic() {
        let ctrl = test_controller();
        {
            let mut st = lock_or_recover(&ctrl.state);
            st.known_peers.insert(
                "b-id".to_string(),
                KnownPeer {
                    id: "b-id".to_string(),
                    name: "Bedroom Speaker".to_string(),
                    host: "192.168.1.20".to_string(),
                    port: 4713,
                    version: VERSION.to_string(),
                    security: "trusted-lan".to_string(),
                    platform: "linux".to_string(),
                    last_seen: 100.0,
                },
            );
            st.known_peers.insert(
                "a-id".to_string(),
                KnownPeer {
                    id: "a-id".to_string(),
                    name: "Room".to_string(),
                    host: "192.168.1.10".to_string(),
                    port: 4713,
                    version: VERSION.to_string(),
                    security: "trusted-lan".to_string(),
                    platform: "linux".to_string(),
                    last_seen: 100.0,
                },
            );
        }
        assert!(resolve_cli_peer(&ctrl, "").is_err());
        assert!(resolve_cli_peer(&ctrl, "   ").is_err());
        let p = resolve_cli_peer(&ctrl, "Room").unwrap();
        assert_eq!(p.id, "a-id");
    }

    #[test]
    fn test_unknown_actions_return_nonzero() {
        let ctrl = test_controller();
        assert_eq!(
            handle_mic(
                &ctrl,
                &MicArgs {
                    mode: "bogus".to_string(),
                    target: None,
                    source: None,
                    latency: None,
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_auto(
                &ctrl,
                &AutoArgs {
                    action: "bogus".to_string(),
                    peer: None,
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_service(
                &ServiceArgs {
                    action: "bogus".to_string(),
                    no_tray: false,
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_firewall(
                &ctrl,
                &FirewallArgs {
                    action: "bogus".to_string(),
                },
                true,
            ),
            1
        );
        assert_eq!(
            handle_config(
                &ctrl,
                &ConfigArgs {
                    action: "bogus".to_string(),
                    key: None,
                    value: None,
                },
                true,
            ),
            1
        );
    }

    #[test]
    fn test_unroute_remove_rule_succeeds_without_active_stream() {
        let ctrl = test_controller();
        lock_or_recover(&ctrl.config)
            .routing
            .always_remote
            .push("firefox".to_string());
        if ipc_is_running(None) {
            let res = ctrl.unroute_app(Some("Firefox"), true);
            assert!(res.is_ok());
        } else {
            let code = handle_unroute(
                &ctrl,
                &UnrouteArgs {
                    app: Some("Firefox".to_string()),
                    all: false,
                    remove_rule: true,
                },
                true,
            );
            assert_eq!(code, 0);
        }
        assert!(lock_or_recover(&ctrl.config)
            .routing
            .always_remote
            .is_empty());
    }

    #[test]
    fn test_config_json_diff_builds_minimal_patch() {
        let mut cfg = Config::default();
        let before = cfg.to_json_value();
        cfg.set_dotted("device.name", "LivingRoom").unwrap();
        let after = cfg.to_json_value();

        let patch = config_json_diff(&before, &after, "device.name");
        assert_eq!(
            patch,
            serde_json::json!({ "device": { "name": "LivingRoom" } })
        );

        let same_patch = config_json_diff(&after, &after, "device.name");
        assert_eq!(
            same_patch,
            serde_json::json!({ "device": { "name": "LivingRoom" } })
        );
    }

    #[test]
    fn test_pair_status_json_redacts_secrets() {
        let ctrl = test_controller();
        let secret_key = "deadbeefcafebabe0123456789abcdef0123456789abcdef0123456789abcdef";
        let secret_cookie = "feedface11223344556677889900aabbccddeeff";
        {
            let mut cfg = lock_or_recover(&ctrl.config);
            cfg.sharing.security = "pin-paired".to_string();
            cfg.sharing.psk_hash = "hashed-psk".to_string();
        }
        {
            let mut st = lock_or_recover(&ctrl.state);
            st.paired_peers.insert(
                "peer-1".to_string(),
                PairedPeer {
                    peer_id: "peer-1".to_string(),
                    name: "Studio Speaker".to_string(),
                    shared_key_hex: secret_key.to_string(),
                    cookie_hex: secret_cookie.to_string(),
                    paired_at: 1_700_000_000.0,
                },
            );
        }

        let out = pair_status_json(&ctrl);
        assert_eq!(out["security"], "pin-paired");
        assert_eq!(out["psk_active"], true);

        let peers = out["paired_peers"].as_array().expect("paired_peers array");
        assert_eq!(peers.len(), 1);
        let peer = &peers[0];
        assert_eq!(peer["peer_id"], "peer-1");
        assert_eq!(peer["name"], "Studio Speaker");
        assert_eq!(peer["paired_at"], 1_700_000_000.0);
        assert_eq!(peer["has_shared_key"], true);
        assert_eq!(peer["has_cookie"], true);
        assert!(peer.get("shared_key_hex").is_none());
        assert!(peer.get("cookie_hex").is_none());

        let serialized = serde_json::to_string(&out).expect("serialize pair status");
        assert!(!serialized.contains("shared_key_hex"));
        assert!(!serialized.contains("cookie_hex"));
        assert!(!serialized.contains(secret_key));
        assert!(!serialized.contains(secret_cookie));

        let code = handle_pair(
            &ctrl,
            &PairArgs {
                target: Some("status".to_string()),
                pin: None,
            },
            true,
        );
        assert_eq!(code, 0);
    }
}
