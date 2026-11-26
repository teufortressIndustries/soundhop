//! Connection, discovery, status, and auto-connect CLI subcommand handlers.

use std::sync::Arc;
use std::time::Duration;

use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::discovery::{discover_once_with_meta, Peer};
use soundhop_core::util::{is_private_address, split_host_port};

use super::try_ipc;
use crate::parser::{AutoArgs, ConnectArgs, DiscoverArgs};
use crate::ui::{emit_error, header, info, lock_or_recover, ok, print_json, sym_arrow};

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

    if let Some(wb) = st.get("web_broadcast") {
        if let Some(line) = format_web_broadcast_status_line(wb) {
            ok(&line);
        }
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

pub(crate) fn format_web_broadcast_status_line(wb: &serde_json::Value) -> Option<String> {
    if wb.is_null() {
        return None;
    }
    let active = wb.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
    if !active {
        return None;
    }
    let http_port = wb.get("http_port").and_then(|v| v.as_u64()).unwrap_or(4717);
    let webrtc_port = wb
        .get("webrtc_udp_port")
        .and_then(|v| v.as_u64())
        .unwrap_or(4718);
    let listeners = wb
        .get("clients")
        .and_then(|v| v.as_array())
        .map_or(0, Vec::len);
    let primary_url = wb
        .get("urls")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            wb.get("localhost_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");
    if primary_url.is_empty() {
        Some(format!(
            "Web Broadcast: Active (HTTP :{http_port}, WebRTC UDP :{webrtc_port}, {listeners} listener(s))"
        ))
    } else {
        Some(format!(
            "Web Broadcast: Active — {primary_url} (HTTP :{http_port}, WebRTC UDP :{webrtc_port}, {listeners} listener(s))"
        ))
    }
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
            let found_with_meta =
                discover_once_with_meta(timeout, cfg.discovery.port, &cfg.device.id, psk);
            let mut found = Vec::with_capacity(found_with_meta.len());
            for (p, meta) in found_with_meta {
                if let Some(m) = meta {
                    controller.remember_peer_v3_meta(&p.id, m);
                }
                controller.remember_peer(&p);
                found.push(p);
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
    let mut scanned_with_meta =
        discover_once_with_meta(0.9, cfg.discovery.port, &cfg.device.id, psk);
    scanned_with_meta.sort_by(|(a, _), (b, _)| a.id.cmp(&b.id));
    let mut scanned = Vec::with_capacity(scanned_with_meta.len());
    for (p, meta) in scanned_with_meta {
        if let Some(m) = meta {
            controller.remember_peer_v3_meta(&p.id, m);
        }
        controller.remember_peer(&p);
        scanned.push(p);
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

#[cfg(test)]
mod tests {
    use soundhop_core::state::KnownPeer;

    use super::*;
    use crate::core_cmds::tests::test_controller;

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
    fn test_format_web_broadcast_status_line() {
        assert_eq!(
            format_web_broadcast_status_line(&serde_json::Value::Null),
            None
        );
        assert_eq!(
            format_web_broadcast_status_line(&serde_json::json!({ "active": false })),
            None
        );

        let active_lan = serde_json::json!({
            "active": true,
            "http_port": 4717,
            "webrtc_udp_port": 4718,
            "urls": ["http://192.168.1.42:4717/#t=abc"],
            "localhost_url": "http://localhost:4717/#t=abc",
            "clients": [{ "id": "c1" }, { "id": "c2" }],
        });
        let line = format_web_broadcast_status_line(&active_lan).unwrap();
        assert!(line.contains("http://192.168.1.42:4717/#t=abc"));
        assert!(line.contains("HTTP :4717"));
        assert!(line.contains("WebRTC UDP :4718"));
        assert!(line.contains("2 listener(s)"));

        let active_localhost_only = serde_json::json!({
            "active": true,
            "http_port": 4800,
            "webrtc_udp_port": 4801,
            "urls": [],
            "localhost_url": "http://localhost:4800/",
            "clients": [],
        });
        let line2 = format_web_broadcast_status_line(&active_localhost_only).unwrap();
        assert!(line2.contains("http://localhost:4800/"));
        assert!(line2.contains("0 listener(s)"));
    }
}
