//! `soundhop android ...` CLI subcommand handlers.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use soundhop_core::controller::Controller;
use soundhop_core::util::{install_signal_handlers, signal_stop_requested};
use soundhop_gui::open_android_wizard;

use crate::parser::AndroidArgs;
use crate::ui::{emit_error, header, info, ok, print_json};

fn parse_bitrate(s: &str) -> Option<u32> {
    let trimmed = s.trim().to_uppercase();
    if trimmed.is_empty() {
        return None;
    }
    let val = if let Some(num) = trimmed.strip_suffix('K') {
        num.parse::<u32>().ok().and_then(|v| v.checked_mul(1000))
    } else if let Some(num) = trimmed.strip_suffix('M') {
        num.parse::<u32>()
            .ok()
            .and_then(|v| v.checked_mul(1_000_000))
    } else {
        trimmed.parse::<u32>().ok()
    }?;
    (val > 0).then_some(val)
}

fn validate_android_source(s: &str) -> Result<String, String> {
    let norm = s.trim().to_lowercase();
    match norm.as_str() {
        "output" | "mic" | "playback" => Ok(norm),
        _ => Err(format!(
            "Invalid --source '{s}'. Expected one of: output, mic, playback."
        )),
    }
}

fn validate_android_codec(s: &str) -> Result<String, String> {
    let norm = s.trim().to_lowercase();
    match norm.as_str() {
        "opus" | "aac" | "flac" | "raw" => Ok(norm),
        _ => Err(format!(
            "Invalid --codec '{s}'. Expected one of: opus, aac, flac, raw."
        )),
    }
}

pub fn handle_android(controller: &Arc<Controller>, args: &AndroidArgs, json_out: bool) -> i32 {
    let action = args.action.trim().to_lowercase();
    match action.as_str() {
        "wizard" | "gui" => match open_android_wizard(Arc::clone(controller)) {
            Ok(()) => {
                if json_out {
                    print_json(&serde_json::json!({ "ok": true }));
                }
                0
            }
            Err(e) => {
                emit_error(&e, json_out);
                1
            }
        },
        "install" => {
            if !json_out {
                info("Downloading and installing managed scrcpy + adb...");
            }
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
                    emit_error(&format!("Failed to install scrcpy: {e}"), json_out);
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
                emit_error(
                    "No USB-connected Android device found to switch to Wi-Fi.",
                    json_out,
                );
                return 1;
            };
            let Some(ip) = controller.android_manager.get_device_wifi_ip(&dev.serial) else {
                emit_error(
                    &format!(
                        "Could not determine Wi-Fi IP address for {}.",
                        dev.display_name()
                    ),
                    json_out,
                );
                return 1;
            };
            let (ok1, msg1) = controller.android_manager.enable_tcpip(&dev.serial, 5555);
            if !ok1 {
                emit_error(&msg1, json_out);
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
                emit_error(&msg2, false);
            }
            if ok2 {
                0
            } else {
                1
            }
        }
        "connect" => {
            let Some(ref target) = args.target else {
                emit_error(
                    "Specify an Android IP[:port] to connect over Wi-Fi.",
                    json_out,
                );
                return 1;
            };
            let (ok_conn, msg) = controller.android_manager.connect_wifi(target, 5555);
            if json_out {
                print_json(&serde_json::json!({ "ok": ok_conn, "message": msg }));
            } else if ok_conn {
                ok(&msg);
            } else {
                emit_error(&msg, false);
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
                emit_error(&msg, false);
            }
            if ok_disc {
                0
            } else {
                1
            }
        }
        "start" => {
            let source = match validate_android_source(&args.source) {
                Ok(s) => s,
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            };
            let codec = match validate_android_codec(&args.codec) {
                Ok(c) => c,
                Err(e) => {
                    emit_error(&e, json_out);
                    return 1;
                }
            };
            let Some(br) = parse_bitrate(&args.bitrate) else {
                emit_error(
                    &format!(
                        "Invalid --bitrate '{}'. Expected a positive number with optional K/M suffix (e.g. 128K).",
                        args.bitrate
                    ),
                    json_out,
                );
                return 1;
            };
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
                        emit_error(
                            "No ready Android device found. Connect via USB or `soundhop android connect <ip>`.",
                            json_out,
                        );
                        return 1;
                    }
                }
            };
            match controller.android_session.start(
                &serial,
                &dev_name,
                Some(&codec),
                Some(&source),
                args.as_mic,
                Some(args.buffer),
                Some(br),
                false,
            ) {
                Ok(st) => {
                    if json_out {
                        print_json(&st);
                    } else {
                        ok(&format!(
                            "Started Android audio stream from {dev_name} (codec={codec}, source={source}, as_mic={})",
                            args.as_mic
                        ));
                    }
                    if args.foreground {
                        install_signal_handlers();
                        if !json_out {
                            info("Streaming in foreground. Press Ctrl-C to stop...");
                        }
                        while controller.android_session.is_active() {
                            if signal_stop_requested() {
                                let _ = controller.android_session.stop();
                                if !json_out {
                                    ok("Stopped Android audio capture session.");
                                }
                                break;
                            }
                            thread::sleep(Duration::from_millis(250));
                        }
                    }
                    0
                }
                Err(e) => {
                    emit_error(&format!("Failed to start Android stream: {e}"), json_out);
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
        "status" => {
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
        other => {
            emit_error(
                &format!(
                    "Unknown android action '{other}'. Expected: start, stop, status, devices, wifi, connect, disconnect, install, wizard."
                ),
                json_out,
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

    #[test]
    fn test_parse_bitrate_and_validators() {
        assert_eq!(parse_bitrate("128K"), Some(128_000));
        assert_eq!(parse_bitrate("2M"), Some(2_000_000));
        assert_eq!(parse_bitrate("96000"), Some(96_000));
        assert_eq!(parse_bitrate("0"), None);
        assert_eq!(parse_bitrate("abc"), None);
        assert_eq!(parse_bitrate(""), None);

        assert_eq!(validate_android_source("OUTPUT").unwrap(), "output");
        assert!(validate_android_source("invalid").is_err());

        assert_eq!(validate_android_codec("Opus").unwrap(), "opus");
        assert!(validate_android_codec("mp3").is_err());
    }

    #[test]
    fn test_android_unknown_action_returns_nonzero() {
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
        let args = AndroidArgs {
            action: "bogus".to_string(),
            target: None,
            source: "output".to_string(),
            codec: "opus".to_string(),
            bitrate: "128K".to_string(),
            buffer: 50,
            as_mic: false,
            foreground: false,
        };
        assert_eq!(handle_android(&ctrl, &args, true), 1);
    }
}
