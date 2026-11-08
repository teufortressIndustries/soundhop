//! `soundhop android ...` CLI subcommand handlers.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use soundhop_core::controller::Controller;
use soundhop_gui::open_android_wizard;

use crate::parser::AndroidArgs;
use crate::ui::{error, header, info, ok, print_json};

fn parse_bitrate(s: &str) -> Option<u32> {
    let trimmed = s.trim().to_uppercase();
    if let Some(num) = trimmed.strip_suffix('K') {
        num.parse::<u32>().ok().map(|v| v * 1000)
    } else if let Some(num) = trimmed.strip_suffix('M') {
        num.parse::<u32>().ok().map(|v| v * 1_000_000)
    } else {
        trimmed.parse::<u32>().ok()
    }
}

pub fn handle_android(controller: &Arc<Controller>, args: &AndroidArgs, json_out: bool) -> i32 {
    let action = args.action.trim().to_lowercase();
    match action.as_str() {
        "wizard" | "gui" => match open_android_wizard(Arc::clone(controller)) {
            Ok(()) => 0,
            Err(e) => {
                error(&e);
                1
            }
        },
        "install" => {
            info("Downloading and installing managed scrcpy + adb...");
            match controller.android_manager.install_scrcpy(None, None) {
                Ok(path) => {
                    let st = controller.android_tool_status();
                    if json_out {
                        print_json(&st);
                    } else {
                        ok(&format!("Installed scrcpy to {}", path.display()));
                    }
                    0
                }
                Err(e) => {
                    error(&format!("Failed to install scrcpy: {e}"));
                    1
                }
            }
        }
        "devices" | "list" => {
            let devs = controller.android_list_devices();
            if json_out {
                print_json(&devs);
                return 0;
            }
            header("Connected Android ADB Devices");
            if devs.is_empty() {
                info("No Android devices detected via ADB.");
            } else {
                for d in &devs {
                    let serial = d.get("serial").and_then(|v| v.as_str()).unwrap_or("");
                    let model = d.get("model").and_then(|v| v.as_str()).unwrap_or("");
                    let state = d.get("state").and_then(|v| v.as_str()).unwrap_or("");
                    let wifi = d.get("is_wifi").and_then(|v| v.as_bool()).unwrap_or(false);
                    ok(&format!(
                        "{serial}  model={model}  transport={}  state={state}",
                        if wifi { "Wi-Fi" } else { "USB" }
                    ));
                }
            }
            0
        }
        "wifi" => {
            let devs = controller.android_manager.list_devices();
            let target_dev = if let Some(ref t) = args.target {
                devs.into_iter().find(|d| d.serial == *t)
            } else {
                devs.into_iter().find(|d| !d.is_wifi && d.is_ready())
            };
            let Some(dev) = target_dev else {
                error("No USB-connected Android device found to switch to Wi-Fi.");
                return 1;
            };
            let Some(ip) = controller.android_manager.get_device_wifi_ip(&dev.serial) else {
                error(&format!(
                    "Could not determine Wi-Fi IP address for {}.",
                    dev.display_name()
                ));
                return 1;
            };
            let (ok1, msg1) = controller.android_manager.enable_tcpip(&dev.serial, 5555);
            if !ok1 {
                error(&msg1);
                return 1;
            }
            thread::sleep(Duration::from_millis(800));
            let (ok2, msg2) = controller.android_manager.connect_wifi(&ip, 5555);
            if json_out {
                print_json(&serde_json::json!({
                    "ok": ok2,
                    "ip": ip,
                    "port": 5555,
                    "message": msg2,
                }));
            } else if ok2 {
                ok(&format!(
                    "Switched {} to Wireless ADB at {ip}:5555 ({msg2})",
                    dev.display_name()
                ));
            } else {
                error(&msg2);
            }
            if ok2 {
                0
            } else {
                1
            }
        }
        "connect" => {
            let Some(ref target) = args.target else {
                error("Specify an Android IP[:port] to connect over Wi-Fi.");
                return 1;
            };
            let (ok_conn, msg) = controller.android_manager.connect_wifi(target, 5555);
            if json_out {
                print_json(&serde_json::json!({ "ok": ok_conn, "message": msg }));
            } else if ok_conn {
                ok(&msg);
            } else {
                error(&msg);
            }
            if ok_conn {
                0
            } else {
                1
            }
        }
        "disconnect" => {
            let (ok_disc, msg) = controller
                .android_manager
                .disconnect_wifi(args.target.as_deref());
            if json_out {
                print_json(&serde_json::json!({ "ok": ok_disc, "message": msg }));
            } else if ok_disc {
                ok(&msg);
            } else {
                error(&msg);
            }
            if ok_disc {
                0
            } else {
                1
            }
        }
        "start" => {
            let devs = controller.android_manager.list_devices();
            let chosen = if let Some(ref t) = args.target {
                devs.iter().find(|d| d.serial == *t || d.ip == *t).cloned()
            } else {
                devs.into_iter().find(|d| d.is_ready())
            };
            let (serial, dev_name) = match chosen {
                Some(d) => (d.serial.clone(), d.display_name()),
                None => {
                    if let Some(ref t) = args.target {
                        (t.clone(), t.clone())
                    } else {
                        error("No ready Android device found. Connect via USB or `soundhop android connect <ip>`.");
                        return 1;
                    }
                }
            };
            let br = parse_bitrate(&args.bitrate);
            match controller.android_session.start(
                &serial,
                &dev_name,
                Some(&args.codec),
                Some(&args.source),
                args.as_mic,
                Some(args.buffer),
                br,
                false,
            ) {
                Ok(st) => {
                    if json_out {
                        print_json(&st);
                    } else {
                        ok(&format!(
                            "Started Android audio stream from {dev_name} (codec={}, source={}, as_mic={})",
                            args.codec, args.source, args.as_mic
                        ));
                    }
                    if args.foreground {
                        info("Streaming in foreground. Press Ctrl-C to stop...");
                        while controller.android_session.is_active() {
                            thread::sleep(Duration::from_millis(250));
                        }
                    }
                    0
                }
                Err(e) => {
                    error(&format!("Failed to start Android stream: {e}"));
                    1
                }
            }
        }
        "stop" => {
            let st = controller.android_session.stop();
            if json_out {
                print_json(&st);
            } else {
                ok("Stopped Android audio capture session.");
            }
            0
        }
        _ => {
            let sess = controller.android_session.status();
            let tools = controller.android_tool_status();
            let devs = controller.android_list_devices();
            if json_out {
                print_json(&serde_json::json!({
                    "session": sess,
                    "tools": tools,
                    "devices": devs,
                }));
            } else {
                header("SoundHop Android Audio Bridge Status");
                let active = sess
                    .get("active")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if active {
                    ok(&format!(
                        "Active Stream: {} (codec={}, source={}, as_mic={})",
                        sess.get("device_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        sess.get("codec").and_then(|v| v.as_str()).unwrap_or(""),
                        sess.get("source").and_then(|v| v.as_str()).unwrap_or(""),
                        sess.get("as_mic")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    ));
                } else {
                    info("Active Stream: None");
                }
                info(&format!("Connected ADB Devices: {}", devs.len()));
            }
            0
        }
    }
}
