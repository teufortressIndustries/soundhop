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
use soundhop_core::discovery::{discover_once, Peer};
use soundhop_core::firewall::{detect as detect_firewall, open_ports};
use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request};
use soundhop_core::security::generate_pairing_pin;
use soundhop_core::service::{
    install as install_service, restart_service, start_service, status as service_status,
    stop_service, uninstall as uninstall_service,
};
use soundhop_core::util::{config_path, is_private_address, split_host_port};

use crate::parser::{
    AutoArgs, Cli, CompletionsArgs, ConfigArgs, ConnectArgs, DiscoverArgs, FirewallArgs, LogsArgs,
    MicArgs, PairArgs, RouteArgs, ServiceArgs, SetupArgs, ShareAction, ShareArgs, ShellChoice,
    UnrouteArgs, VolumeArgs,
};
use crate::ui::{error, header, info, ok, print_json, safe_print, sym_arrow, warning};

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
            controller.config.lock().unwrap().device.name = name.trim().to_string();
        }
    }
    controller.config.lock().unwrap().sharing.enabled = true;
    controller.save();

    let share_res = controller.share(true, Some(false));
    let cfg = controller.config.lock().unwrap().clone();

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

    if json_out {
        print_json(&serde_json::json!({
            "ok": share_res.is_ok(),
            "device": cfg.device.name,
            "sharing": share_res.ok(),
            "firewall": fw_res.map(|f| f.to_json()),
            "service": svc_res,
        }));
        return 0;
    }

    header(&format!("SoundHop v{VERSION} Setup Complete"));
    ok(&format!("Device Name: {}", cfg.device.name));
    match share_res {
        Ok(_) => ok(&format!(
            "Speaker Sharing: Enabled on TCP port {}",
            cfg.sharing.port
        )),
        Err(e) => warning(&format!("Speaker Sharing: {e}")),
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
    if svc_res.is_some() {
        ok("Background Daemon & Tray Autostart: Installed");
    }
    0
}

pub fn handle_status(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let st = if ipc_is_running(None) {
        ipc_request(
            "status",
            serde_json::json!({}),
            None,
            Duration::from_secs(2),
        )
        .unwrap_or_else(|_| controller.status(None))
    } else {
        controller.status(None)
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
    let peers: Vec<Peer> = if ipc_is_running(None) {
        if let Ok(resp) = ipc_request(
            "discover",
            serde_json::json!({}),
            None,
            Duration::from_secs(3),
        ) {
            resp.get("peers")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| serde_json::from_value::<Peer>(item.clone()).ok())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        let cfg = controller.config.lock().unwrap().clone();
        let found = discover_once(args.timeout.max(0.3), cfg.discovery.port, &cfg.device.id);
        for p in &found {
            controller.remember_peer(p);
        }
        controller.save();
        found
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
    let cfg = controller.config.lock().unwrap().clone();
    let q = target.trim();
    if let Some(kp) = controller.known_peer(q) {
        return Ok(kp);
    }
    let st = controller.state.lock().unwrap().clone();
    for kp in st.known_peers.values() {
        if kp.name.eq_ignore_ascii_case(q)
            || kp.host.eq_ignore_ascii_case(q)
            || kp.name.to_lowercase().contains(&q.to_lowercase())
        {
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
    let scanned = discover_once(0.9, cfg.discovery.port, &cfg.device.id);
    for p in scanned {
        controller.remember_peer(&p);
        if p.id.eq_ignore_ascii_case(q)
            || p.name.eq_ignore_ascii_case(q)
            || p.host.eq_ignore_ascii_case(q)
            || p.name.to_lowercase().contains(&q.to_lowercase())
        {
            return Ok(p);
        }
    }
    Err(format!("Speaker '{q}' not found on LAN."))
}

pub fn handle_connect(controller: &Arc<Controller>, args: &ConnectArgs, json_out: bool) -> i32 {
    if let Some(lat) = args.latency {
        controller.config.lock().unwrap().connection.latency_ms = Some(lat);
    }
    let switch_default = if args.default_sink {
        Some(true)
    } else if args.no_default {
        Some(false)
    } else {
        None
    };

    if ipc_is_running(None) {
        let payload = if args.targets.len() > 1 || args.combine {
            serde_json::json!({
                "peers": args.targets,
                "keep_local": args.keep_local,
                "switch_default": switch_default,
            })
        } else {
            serde_json::json!({
                "peer": args.targets.first().cloned().unwrap_or_default(),
                "keep_local": args.keep_local,
                "switch_default": switch_default,
            })
        };
        match ipc_request("connect", payload, None, Duration::from_secs(6)) {
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
                return 0;
            }
            Err(e) => {
                error(&format!("{e}"));
                return 1;
            }
        }
    }

    let mut peers = Vec::new();
    if args.targets.is_empty() {
        let last = controller
            .config
            .lock()
            .unwrap()
            .connection
            .last_peer
            .clone();
        if !last.is_empty() {
            if let Ok(p) = resolve_cli_peer(controller, &last) {
                peers.push(p);
            }
        }
        if peers.is_empty() {
            error("Specify a peer name, ID, or IP to connect to.");
            return 1;
        }
    } else {
        for t in &args.targets {
            match resolve_cli_peer(controller, t) {
                Ok(p) => peers.push(p),
                Err(e) => {
                    error(&e);
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
                controller.config.lock().unwrap().connection.last_peer = peers[0].id.clone();
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
            error(&format!("{e}"));
            1
        }
    }
}

pub fn handle_disconnect(controller: &Arc<Controller>, json_out: bool) -> i32 {
    if ipc_is_running(None) {
        if let Ok(resp) = ipc_request(
            "disconnect",
            serde_json::json!({}),
            None,
            Duration::from_secs(4),
        ) {
            if json_out {
                print_json(&resp);
            } else {
                ok("Disconnected from remote speaker(s).");
            }
            return 0;
        }
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
            error(&format!("{e}"));
            1
        }
    }
}

pub fn handle_share(controller: &Arc<Controller>, args: &ShareArgs, json_out: bool) -> i32 {
    if let Some(p) = args.port {
        controller.config.lock().unwrap().sharing.port = p;
    }
    let enable_opt: Option<bool> = match &args.action {
        Some(ShareAction::On { port }) => {
            if let Some(p) = port {
                controller.config.lock().unwrap().sharing.port = *p;
            }
            Some(true)
        }
        Some(ShareAction::Off) => Some(false),
        Some(ShareAction::Status) | None => None,
    };

    if let Some(enable) = enable_opt {
        if ipc_is_running(None) {
            if let Ok(resp) = ipc_request(
                "share",
                serde_json::json!({ "enabled": enable }),
                None,
                Duration::from_secs(4),
            ) {
                if json_out {
                    print_json(&resp);
                } else if enable {
                    ok("Speaker sharing enabled.");
                } else {
                    ok("Speaker sharing disabled.");
                }
                return 0;
            }
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
                error(&format!("{e}"));
                1
            }
        }
    } else {
        let cfg = controller.config.lock().unwrap().clone();
        let st = controller.state.lock().unwrap().clone();
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

pub fn handle_pair(controller: &Arc<Controller>, args: &PairArgs, json_out: bool) -> i32 {
    let target = args.target.as_deref().unwrap_or("status").trim();
    if target.eq_ignore_ascii_case("generate") {
        let pin = generate_pairing_pin();
        if json_out {
            print_json(&serde_json::json!({ "pin": pin }));
        } else {
            ok(&format!("Generated 6-digit pairing PIN: {pin}"));
        }
        return 0;
    }
    if target.eq_ignore_ascii_case("clear") {
        {
            let mut cfg = controller.config.lock().unwrap();
            cfg.sharing.psk_hash.clear();
            cfg.sharing.security = "trusted-lan".to_string();
        }
        controller.state.lock().unwrap().paired_peers.clear();
        controller.save();
        if json_out {
            print_json(&serde_json::json!({ "cleared": true }));
        } else {
            ok("Cleared PIN/PSK pairing and reverted to trusted-lan mode.");
        }
        return 0;
    }
    if target.eq_ignore_ascii_case("status") && args.pin.is_none() {
        let st = controller.state.lock().unwrap().clone();
        let cfg = controller.config.lock().unwrap().clone();
        let out = serde_json::json!({
            "security": cfg.sharing.security,
            "psk_active": !cfg.sharing.psk_hash.is_empty(),
            "paired_peers": st.paired_peers.values().cloned().collect::<Vec<_>>(),
        });
        if json_out {
            print_json(&out);
        } else {
            header("SoundHop PIN / PSK Pairing Status");
            info(&format!("Security Mode: {}", cfg.sharing.security));
            info(&format!("Paired Peers: {}", st.paired_peers.len()));
        }
        return 0;
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
            error(&format!("{e}"));
            1
        }
    }
}

pub fn handle_apps(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let apps = controller.list_apps();
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
            error(&format!("{e}"));
            1
        }
    }
}

pub fn handle_unroute(controller: &Arc<Controller>, args: &UnrouteArgs, json_out: bool) -> i32 {
    if args.all || args.app.is_none() {
        controller.state.lock().unwrap().routed_apps.clear();
        if args.remove_rule {
            let mut cfg = controller.config.lock().unwrap();
            cfg.routing.always_remote.clear();
            cfg.routing.always_local.clear();
        }
        controller.save();
        if json_out {
            print_json(&serde_json::json!({ "unrouted_all": true }));
        } else {
            ok("Cleared per-app routing overrides.");
        }
        return 0;
    }

    let app = args.app.as_deref().unwrap_or("");
    if args.remove_rule {
        let key = app.trim().to_lowercase();
        let mut cfg = controller.config.lock().unwrap();
        cfg.routing.always_remote.retain(|x| x != &key);
        cfg.routing.always_local.retain(|x| x != &key);
    }
    match controller.route_app(app, Some("local"), false) {
        Ok(val) => {
            if json_out {
                print_json(&val);
            } else {
                ok(&format!("Restored '{app}' to local output."));
            }
            0
        }
        Err(e) => {
            error(&format!("{e}"));
            1
        }
    }
}

pub fn handle_mic(controller: &Arc<Controller>, args: &MicArgs, json_out: bool) -> i32 {
    let mode = args.mode.trim().to_lowercase();
    match mode.as_str() {
        "send" | "tx" | "on" => match controller.enable_mic("send") {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok("Sending local microphone to remote peer.");
                }
                0
            }
            Err(e) => {
                error(&format!("{e}"));
                1
            }
        },
        "receive" | "rx" => match controller.enable_mic("receive") {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok("Receiving microphone from remote peer.");
                }
                0
            }
            Err(e) => {
                error(&format!("{e}"));
                1
            }
        },
        "off" | "stop" | "disable" => match controller.disable_mic() {
            Ok(val) => {
                if json_out {
                    print_json(&val);
                } else {
                    ok("Microphone pass-through disabled.");
                }
                0
            }
            Err(e) => {
                error(&format!("{e}"));
                1
            }
        },
        _ => {
            let st = controller.state.lock().unwrap().clone();
            let out = serde_json::json!({
                "enabled": !st.connected_source.is_empty(),
                "direction": st.mic_direction,
                "source": st.connected_source,
            });
            if json_out {
                print_json(&out);
            } else {
                info(&format!(
                    "Microphone Pass-Through: {} ({})",
                    if st.connected_source.is_empty() {
                        "off"
                    } else {
                        "active"
                    },
                    if st.mic_direction.is_empty() {
                        "none"
                    } else {
                        &st.mic_direction
                    }
                ));
            }
            0
        }
    }
}

pub fn handle_volume(controller: &Arc<Controller>, args: &VolumeArgs, json_out: bool) -> i32 {
    let Some(ref raw) = args.value else {
        let v = controller.get_volume();
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
    let res = match val_str.as_str() {
        "mute" => controller.set_volume(None, Some(true), true),
        "unmute" => controller.set_volume(None, Some(false), true),
        "toggle" => {
            let cur = controller
                .get_volume()
                .get("mute")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            controller.set_volume(None, Some(!cur), true)
        }
        other => {
            let cur_vol = controller
                .get_volume()
                .get("volume")
                .and_then(|x| x.as_u64())
                .unwrap_or(100) as i32;
            let clean = other.trim_end_matches('%');
            let target_vol = if let Some(delta) = clean.strip_prefix('+') {
                cur_vol + delta.parse::<i32>().unwrap_or(0)
            } else if let Some(delta) = clean.strip_prefix('-') {
                cur_vol - delta.parse::<i32>().unwrap_or(0)
            } else {
                clean.parse::<i32>().unwrap_or(cur_vol)
            };
            controller.set_volume(Some(target_vol.clamp(0, 150) as u32), None, true)
        }
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
            let v = controller.set_auto(true);
            if json_out {
                print_json(&v);
            } else {
                ok("Auto-connect enabled.");
            }
            0
        }
        "off" | "disable" => {
            let v = controller.set_auto(false);
            if json_out {
                print_json(&v);
            } else {
                ok("Auto-connect disabled.");
            }
            0
        }
        "prefer" => {
            if let Some(ref p) = args.peer {
                controller.config.lock().unwrap().connection.last_peer = p.trim().to_string();
                controller.save();
                if json_out {
                    print_json(&serde_json::json!({ "preferred_peer": p.trim() }));
                } else {
                    ok(&format!(
                        "Preferred auto-connect peer set to '{}'.",
                        p.trim()
                    ));
                }
                0
            } else {
                error("Specify a peer ID or name to prefer.");
                1
            }
        }
        _ => {
            let cfg = controller.config.lock().unwrap().clone();
            let st = controller.state.lock().unwrap().clone();
            let out = serde_json::json!({
                "auto_connect": cfg.connection.auto_connect,
                "paused": st.auto_paused,
                "preferred_peer": cfg.connection.last_peer,
            });
            if json_out {
                print_json(&out);
            } else {
                info(&format!(
                    "Auto-Connect: {} (paused: {}, preferred: '{}')",
                    cfg.connection.auto_connect, st.auto_paused, cfg.connection.last_peer
                ));
            }
            0
        }
    }
}

pub fn handle_daemon(verbose: bool) -> i32 {
    let daemon = Daemon::new(None, None, !verbose, None);
    match daemon.run_until_stopped() {
        Ok(()) => 0,
        Err(e) => {
            error(&e);
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
    if ipc_is_running(None)
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
    let res = match act.as_str() {
        "install" => install_service(true, true, !args.no_tray),
        "uninstall" | "remove" => uninstall_service(),
        "start" => serde_json::json!({ "started": start_service() }),
        "stop" => serde_json::json!({ "stopped": stop_service() }),
        "restart" => serde_json::json!({ "restarted": restart_service() }),
        _ => service_status(),
    };
    if json_out {
        print_json(&res);
    } else {
        safe_print(&serde_json::to_string_pretty(&res).unwrap_or_default());
    }
    0
}

pub fn handle_firewall(controller: &Arc<Controller>, args: &FirewallArgs, json_out: bool) -> i32 {
    let cfg = controller.config.lock().unwrap().clone();
    let act = args.action.trim().to_lowercase();
    if act == "open" {
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
            error(&format!(
                "Firewall ({}): {}",
                res.firewall.kind,
                res.errors.join("; ")
            ));
        }
        return if res.ok { 0 } else { 1 };
    }
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
            let _ = def.save(None);
            *controller.config.lock().unwrap() = def.clone();
            if json_out {
                print_json(&def.to_json_value());
            } else {
                ok("Reset configuration to defaults.");
            }
            0
        }
        "get" => {
            let Some(ref key) = args.key else {
                error("Specify a dotted key to read (e.g. `device.name`).");
                return 1;
            };
            let cfg = controller.config.lock().unwrap().clone();
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
                    error(&format!("{e}"));
                    1
                }
            }
        }
        "set" => {
            let (Some(ref key), Some(ref val)) = (&args.key, &args.value) else {
                error("Usage: `soundhop config set <section.key> <value>`");
                return 1;
            };
            let mut cfg = controller.config.lock().unwrap().clone();
            match cfg.set_dotted(key, val) {
                Ok(()) => {
                    let _ = cfg.save(None);
                    *controller.config.lock().unwrap() = cfg;
                    if json_out {
                        print_json(&serde_json::json!({ "ok": true, "key": key, "value": val }));
                    } else {
                        ok(&format!("Updated {key} = {val}"));
                    }
                    0
                }
                Err(e) => {
                    error(&format!("{e}"));
                    1
                }
            }
        }
        _ => {
            let cfg = controller.config.lock().unwrap().clone();
            if json_out {
                print_json(&cfg.to_json_value());
            } else {
                safe_print(&cfg.to_toml_string());
            }
            0
        }
    }
}

pub fn handle_logs(controller: &Arc<Controller>, args: &LogsArgs, json_out: bool) -> i32 {
    let sess = controller.android_session.status();
    let logs = sess
        .get("recent_logs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if json_out {
        print_json(&serde_json::json!({
            "lines": args.lines,
            "logs": logs,
        }));
    } else if logs.is_empty() {
        info("No recent session logs recorded.");
    } else {
        for l in logs.iter().rev().take(args.lines).rev() {
            if let Some(s) = l.as_str() {
                safe_print(s);
            }
        }
    }
    0
}
