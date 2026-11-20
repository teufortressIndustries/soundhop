//! Speaker sharing and PIN/PSK pairing CLI subcommand handlers.

use std::sync::Arc;
use std::time::Duration;

use soundhop_core::controller::Controller;
use soundhop_core::security::generate_pairing_pin;
use soundhop_core::state::PairedPeer;

use super::try_ipc;
use crate::parser::{PairArgs, ShareAction, ShareArgs};
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
                target: Some("status".to_string()),
                pin: None,
            },
            true,
        );
        assert_eq!(code, 0);
    }
}
