//! Configuration and log inspection CLI subcommand handlers.

use std::sync::Arc;
use std::time::Duration;

use soundhop_core::config::Config;
use soundhop_core::controller::Controller;
use soundhop_core::util::{config_path, log_path};

use super::try_ipc;
use crate::parser::{ConfigArgs, LogsArgs};
use crate::ui::{emit_error, info, lock_or_recover, ok, print_json, safe_print};

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

fn read_log_file_tail(path: &std::path::Path, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(max_lines);
    all[start..].iter().map(|s| (*s).to_string()).collect()
}

pub fn handle_logs(controller: &Arc<Controller>, args: &LogsArgs, json_out: bool) -> i32 {
    let daemon_log_path = log_path();
    let daemon_log_str = daemon_log_path.to_string_lossy().into_owned();

    if let Some(ipc_res) = try_ipc(
        "logs",
        serde_json::json!({ "lines": args.lines }),
        Duration::from_secs(4),
    ) {
        return match ipc_res {
            Ok(resp) => {
                let mut all_lines: Vec<String> = resp
                    .get("lines")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.as_str().map(ToString::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if all_lines.is_empty() {
                    all_lines = read_log_file_tail(&daemon_log_path, args.lines);
                }
                let start = all_lines.len().saturating_sub(args.lines);
                let tail = &all_lines[start..];
                if json_out {
                    print_json(&serde_json::json!({
                        "ok": true,
                        "lines": args.lines,
                        "log_path": daemon_log_str,
                        "logs": tail,
                    }));
                } else if tail.is_empty() {
                    info(&format!(
                        "No recent daemon/session logs recorded (log file: {daemon_log_str})."
                    ));
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

    let file_lines = read_log_file_tail(&daemon_log_path, args.lines);
    if !file_lines.is_empty() {
        if json_out {
            print_json(&serde_json::json!({
                "ok": true,
                "daemon_running": false,
                "lines": args.lines,
                "log_path": daemon_log_str,
                "logs": file_lines,
            }));
        } else {
            for line in &file_lines {
                safe_print(line);
            }
        }
        return 0;
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
            "log_path": daemon_log_str,
            "logs": logs,
        }));
    } else if logs.is_empty() {
        #[cfg(windows)]
        info(&format!(
            "SoundHop daemon is not running and no log file was found at {daemon_log_str} (default: %LOCALAPPDATA%\\soundhop\\soundhop.log)."
        ));
        #[cfg(not(windows))]
        info(&format!(
            "SoundHop daemon is not running; no recent session logs available (log file: {daemon_log_str})."
        ));
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
    fn test_read_log_file_tail() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("soundhop.log");
        assert!(read_log_file_tail(&file, 10).is_empty());

        std::fs::write(&file, "line1\nline2\nline3\nline4\n").unwrap();
        assert_eq!(read_log_file_tail(&file, 2), vec!["line3", "line4"]);
        assert_eq!(
            read_log_file_tail(&file, 10),
            vec!["line1", "line2", "line3", "line4"]
        );
        assert!(read_log_file_tail(&file, 0).is_empty());
    }
}
