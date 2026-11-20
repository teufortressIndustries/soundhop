//! Speaker sharing and PIN/PSK pairing CLI subcommand handlers.

use std::sync::Arc;
use std::time::Duration;

use soundhop_core::controller::Controller;
use soundhop_core::security::generate_pairing_pin;
use soundhop_core::state::PairedPeer;

use super::try_ipc;
use crate::parser::{PairAction, PairArgs, ShareAction, ShareArgs};
use crate::ui::{emit_error, header, info, lock_or_recover, ok, print_json};

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
    if let Some(ref action) = args.action {
        return handle_pair_v2(controller, action, json_out);
    }

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
        let res = controller.clear_pairing();
        if json_out {
            print_json(&res);
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

fn handle_pair_v2(controller: &Arc<Controller>, action: &PairAction, json_out: bool) -> i32 {
    match action {
        PairAction::Request { host, port, code } => {
            let mut payload = serde_json::json!({ "host": host });
            if let Some(p) = port {
                payload["port"] = serde_json::json!(p);
            }
            if let Some(c) = code {
                payload["code"] = serde_json::json!(c);
            }
            let res =
                if let Some(ipc_res) = try_ipc("pair_request", payload, Duration::from_secs(65)) {
                    ipc_res
                } else {
                    controller
                        .pair_request(host, *port, code.as_deref())
                        .map_err(|e| e.to_string())
                };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        let name = val
                            .get("peer")
                            .and_then(|p| p.get("display_name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or(host);
                        let fp = val
                            .get("peer")
                            .and_then(|p| p.get("fingerprint"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let sas = val.get("sas_code").and_then(|v| v.as_str()).unwrap_or("");
                        ok(&format!(
                            "Paired with '{name}' (code: {sas}, fingerprint: {fp})."
                        ));
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
        PairAction::Pending => {
            let res = if let Some(ipc_res) = try_ipc(
                "pair_pending",
                serde_json::json!({}),
                Duration::from_secs(3),
            ) {
                ipc_res
            } else {
                let reqs = controller.pending_pair_requests();
                Ok(serde_json::json!({ "ok": true, "requests": reqs }))
            };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        let reqs = val
                            .get("requests")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        header("Pending Pair Requests");
                        if reqs.is_empty() {
                            info("No pending pair requests.");
                        } else {
                            for r in &reqs {
                                let id = r.get("request_id").and_then(|v| v.as_str()).unwrap_or("");
                                let name = r
                                    .get("display_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unknown");
                                let sas = r.get("sas_code").and_then(|v| v.as_str()).unwrap_or("");
                                let addr =
                                    r.get("remote_addr").and_then(|v| v.as_str()).unwrap_or("");
                                info(&format!(
                                    "  {id}  {name} ({addr}) — Verification Code: {sas}"
                                ));
                            }
                        }
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
        PairAction::Approve { id } => {
            let res = if let Some(ipc_res) = try_ipc(
                "pair_approve",
                serde_json::json!({ "request_id": id }),
                Duration::from_secs(5),
            ) {
                ipc_res
            } else {
                controller
                    .approve_pair_request(id)
                    .map_err(|e| e.to_string())
            };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        ok(&format!("Approved pair request '{id}'."));
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
        PairAction::Deny { id } => {
            let res = if let Some(ipc_res) = try_ipc(
                "pair_deny",
                serde_json::json!({ "request_id": id }),
                Duration::from_secs(5),
            ) {
                ipc_res
            } else {
                controller.deny_pair_request(id).map_err(|e| e.to_string())
            };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        ok(&format!("Denied pair request '{id}'."));
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
        PairAction::Revoke { device_id } => {
            let res = if let Some(ipc_res) = try_ipc(
                "pair_revoke",
                serde_json::json!({ "device_id": device_id }),
                Duration::from_secs(5),
            ) {
                ipc_res
            } else {
                controller
                    .revoke_trusted_peer(device_id)
                    .map_err(|e| e.to_string())
            };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        let revoked = val
                            .get("revoked")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if revoked {
                            ok(&format!("Revoked trusted device '{device_id}'."));
                        } else {
                            info(&format!("No trusted device found with ID '{device_id}'."));
                        }
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
        PairAction::List => {
            let res = if let Some(ipc_res) =
                try_ipc("pair_list", serde_json::json!({}), Duration::from_secs(3))
            {
                ipc_res
            } else {
                let peers: Vec<serde_json::Value> = controller
                    .trusted_peers()
                    .iter()
                    .map(|p| p.to_public_json())
                    .collect();
                Ok(serde_json::json!({ "ok": true, "trusted_peers": peers }))
            };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        let peers = val
                            .get("trusted_peers")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        header("Trusted Devices");
                        if peers.is_empty() {
                            info("No trusted devices paired.");
                        } else {
                            for p in &peers {
                                let name = p
                                    .get("display_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unknown");
                                let id = p.get("device_id").and_then(|v| v.as_str()).unwrap_or("");
                                let fp =
                                    p.get("fingerprint").and_then(|v| v.as_str()).unwrap_or("");
                                info(&format!("  {name}  (id: {id}, fingerprint: {fp})"));
                            }
                        }
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
        PairAction::Code => {
            let res = if let Some(ipc_res) =
                try_ipc("pair_code", serde_json::json!({}), Duration::from_secs(3))
            {
                ipc_res
            } else {
                Ok(controller.generate_headless_pair_code())
            };
            match res {
                Ok(val) => {
                    if json_out {
                        print_json(&val);
                    } else {
                        let code = val.get("code").and_then(|v| v.as_str()).unwrap_or("");
                        let ttl = val.get("ttl_secs").and_then(|v| v.as_u64()).unwrap_or(120);
                        ok(&format!(
                            "One-time headless pairing code: {code} (valid for {ttl}s)"
                        ));
                        info(&format!(
                            "On the remote device, run: soundhop pair request <this-host> --code {code}"
                        ));
                    }
                    0
                }
                Err(e) => {
                    emit_error(&e, json_out);
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_cmds::tests::test_controller;

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
                action: None,
                target: Some("status".to_string()),
                pin: None,
            },
            true,
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn test_pair_v2_cli_subcommands_parse_and_execute() {
        use crate::parser::{Cli, Commands};
        use clap::Parser;

        let parsed = Cli::try_parse_from([
            "soundhop",
            "pair",
            "request",
            "192.168.1.50",
            "--port",
            "4725",
            "--code",
            "123456",
        ])
        .expect("parse pair request");
        match parsed.command {
            Some(Commands::Pair(pa)) => match pa.action {
                Some(PairAction::Request { host, port, code }) => {
                    assert_eq!(host, "192.168.1.50");
                    assert_eq!(port, Some(4725));
                    assert_eq!(code.as_deref(), Some("123456"));
                }
                other => panic!("unexpected pair action: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }

        for argv in [
            vec!["soundhop", "pair", "pending"],
            vec!["soundhop", "pair", "approve", "req-1"],
            vec!["soundhop", "pair", "deny", "req-1"],
            vec!["soundhop", "pair", "revoke", "dev-1"],
            vec!["soundhop", "pair", "list"],
            vec!["soundhop", "pair", "code"],
            vec!["soundhop", "pair", "status"],
            vec!["soundhop", "pair", "my-pc", "--pin", "ABCDEF123456"],
        ] {
            assert!(
                Cli::try_parse_from(argv.clone()).is_ok(),
                "failed to parse {argv:?}"
            );
        }

        let ctrl = test_controller();
        assert_eq!(
            handle_pair(
                &ctrl,
                &PairArgs {
                    action: Some(PairAction::Code),
                    target: None,
                    pin: None,
                },
                true,
            ),
            0
        );
        assert_eq!(
            handle_pair(
                &ctrl,
                &PairArgs {
                    action: Some(PairAction::List),
                    target: None,
                    pin: None,
                },
                true,
            ),
            0
        );
        assert_eq!(
            handle_pair(
                &ctrl,
                &PairArgs {
                    action: Some(PairAction::Pending),
                    target: None,
                    pin: None,
                },
                true,
            ),
            0
        );
    }
}
