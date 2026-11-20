//! Per-app audio routing, microphone pass-through, and volume CLI subcommand handlers.

use std::sync::Arc;
use std::time::Duration;

use soundhop_core::controller::Controller;

use super::try_ipc;
use crate::parser::{MicArgs, RouteArgs, UnrouteArgs, VolumeArgs};
use crate::ui::{emit_error, header, info, lock_or_recover, ok, print_json, sym_arrow};

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

#[cfg(test)]
mod tests {
    use soundhop_core::ipc::is_running as ipc_is_running;

    use super::*;
    use crate::core_cmds::tests::test_controller;

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
}
