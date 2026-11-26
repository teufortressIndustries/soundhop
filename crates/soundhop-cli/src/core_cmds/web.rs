//! `soundhop web ...` / `soundhop broadcast ...` CLI subcommand handlers.

use std::sync::Arc;
use std::time::Duration;

use soundhop_core::controller::Controller;
use soundhop_core::web::qr::render_terminal_qr;
use soundhop_core::web::{WebBroadcastStatus, WebClientInfo};

use super::try_ipc;
use crate::parser::{WebAction, WebArgs};
use crate::ui::{emit_error, header, info, ok, print_json, safe_print};

const DAEMON_REQUIRED_MSG: &str =
    "SoundHop daemon is not running. Start it with `soundhop daemon` or `soundhop service start`.";

/// Format a byte count into a compact human-readable string (`B`, `KiB`, `MiB`, `GiB`).
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else if (bytes as f64) < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.2} GiB", bytes as f64 / GIB)
    }
}

/// Select the primary browser listen URL from `status`, preferring `localhost_url` when `prefer_adb` is set.
pub(crate) fn primary_listen_url(status: &WebBroadcastStatus, prefer_adb: bool) -> Option<String> {
    if prefer_adb {
        (!status.localhost_url.is_empty())
            .then(|| status.localhost_url.clone())
            .or_else(|| status.urls.first().cloned())
    } else {
        status
            .urls
            .first()
            .cloned()
            .or_else(|| (!status.localhost_url.is_empty()).then(|| status.localhost_url.clone()))
    }
}

fn parse_web_status_from_ipc(resp: &serde_json::Value) -> Result<WebBroadcastStatus, String> {
    let wb = resp
        .get("web_broadcast")
        .ok_or_else(|| "Daemon response missing `web_broadcast` payload.".to_string())?;
    serde_json::from_value::<WebBroadcastStatus>(wb.clone())
        .map_err(|e| format!("Failed to parse `web_broadcast` status: {e}"))
}

fn build_web_start_payload(
    port: Option<u16>,
    keep_local: bool,
    no_keep_local: bool,
    no_token: bool,
    adb: bool,
    serial: Option<&str>,
    open: bool,
) -> serde_json::Value {
    let mut payload = serde_json::json!({});
    if let Some(p) = port {
        payload["port"] = serde_json::json!(p);
    }
    if keep_local {
        payload["keep_local"] = serde_json::json!(true);
    } else if no_keep_local {
        payload["keep_local"] = serde_json::json!(false);
    }
    if no_token {
        payload["require_token"] = serde_json::json!(false);
    }
    let clean_serial = serial.map(str::trim).filter(|s| !s.is_empty());
    if adb || open || clean_serial.is_some() {
        payload["adb_reverse"] = serde_json::json!(true);
    }
    if let Some(s) = clean_serial {
        payload["serial"] = serde_json::json!(s);
    }
    if open {
        payload["open_browser"] = serde_json::json!(true);
    }
    payload
}

fn transport_badge(transport: &str) -> &'static str {
    if transport.eq_ignore_ascii_case("webrtc") {
        "[WebRTC]"
    } else if transport.eq_ignore_ascii_case("ws") {
        "[WS]"
    } else {
        "[OTHER]"
    }
}

fn format_listener_line(client: &WebClientInfo) -> String {
    let badge = transport_badge(&client.transport);
    let rtt = client
        .rtt_ms
        .map(|r| format!("{r:.1} ms"))
        .unwrap_or_else(|| "-".to_string());
    let ua = if client.user_agent.trim().is_empty() {
        ""
    } else {
        client.user_agent.trim()
    };
    if ua.is_empty() {
        format!(
            "  {}  {:<8}  {:<21}  RTT: {:<8}  Sent: {:<10}  Drops: {}",
            client.id,
            badge,
            client.remote_addr,
            rtt,
            format_bytes(client.bytes_sent),
            client.dropped_frames
        )
    } else {
        format!(
            "  {}  {:<8}  {:<21}  RTT: {:<8}  Sent: {:<10}  Drops: {}  ({})",
            client.id,
            badge,
            client.remote_addr,
            rtt,
            format_bytes(client.bytes_sent),
            client.dropped_frames,
            ua
        )
    }
}

fn print_web_status_human(status: &WebBroadcastStatus, daemon_running: bool) {
    header("SoundHop Web Broadcast Status");
    if !status.active {
        info("State: Inactive");
        if !daemon_running {
            info(DAEMON_REQUIRED_MSG);
        } else {
            info("Start broadcasting with `soundhop web start`.");
        }
        return;
    }

    ok(&format!(
        "State: Active (HTTP/WS :{}, WebRTC UDP :{})",
        status.http_port, status.webrtc_udp_port
    ));
    let capture_state = if status.pipeline_running {
        "running"
    } else {
        "idle (starts on first listener)"
    };
    if status.capture_source.is_empty() {
        info(&format!("Capture Pipeline: {capture_state}"));
    } else {
        info(&format!(
            "Capture Pipeline: {capture_state} ({})",
            status.capture_source
        ));
    }
    info(&format!(
        "Keep Local Speakers: {}",
        if status.keep_local { "yes" } else { "no" }
    ));
    if status.token_required {
        info(&format!(
            "Session Token: {}",
            status.token.as_deref().unwrap_or("<hidden>")
        ));
    } else {
        info("Session Token: Disabled (--no-token)");
    }

    if status.urls.is_empty() {
        info("LAN URLs: (none detected)");
    } else {
        for url in &status.urls {
            info(&format!("LAN URL: {url}"));
        }
    }
    if !status.localhost_url.is_empty() {
        info(&format!(
            "Localhost URL (USB ADB): {}",
            status.localhost_url
        ));
    }
    if !status.adb_reversed_serials.is_empty() {
        ok(&format!(
            "ADB Reverse Active: {}",
            status.adb_reversed_serials.join(", ")
        ));
    }

    info(&format!("Listeners ({}):", status.clients.len()));
    if status.clients.is_empty() {
        info("  (none connected)");
    } else {
        for client in &status.clients {
            safe_print(&format_listener_line(client));
        }
    }
}

fn print_qr_output(url: &str, status: &WebBroadcastStatus, adb: bool) -> Result<(), String> {
    let qr = render_terminal_qr(url).map_err(|e| e.to_string())?;
    safe_print(&qr);
    info(&format!("Listen URL: {url}"));
    if !adb && status.urls.len() > 1 {
        for extra in &status.urls[1..] {
            info(&format!("Alternate URL: {extra}"));
        }
    }
    Ok(())
}

pub fn handle_web(controller: &Arc<Controller>, args: &WebArgs, json_out: bool) -> i32 {
    match &args.action {
        None | Some(WebAction::Status) => {
            match try_ipc("web_status", serde_json::json!({}), Duration::from_secs(3)) {
                Some(Ok(resp)) => {
                    if json_out {
                        print_json(&resp);
                        return 0;
                    }
                    match parse_web_status_from_ipc(&resp) {
                        Ok(st) => {
                            print_web_status_human(&st, true);
                            0
                        }
                        Err(e) => {
                            emit_error(&e, false);
                            1
                        }
                    }
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    1
                }
                None => {
                    let st = controller.web_broadcast_status();
                    if json_out {
                        print_json(&serde_json::json!({
                            "ok": true,
                            "daemon_running": false,
                            "web_broadcast": st,
                        }));
                    } else {
                        print_web_status_human(&st, false);
                    }
                    0
                }
            }
        }
        Some(WebAction::Start {
            port,
            keep_local,
            no_keep_local,
            no_token,
            adb,
            serial,
            open,
        }) => {
            let payload = build_web_start_payload(
                *port,
                *keep_local,
                *no_keep_local,
                *no_token,
                *adb,
                serial.as_deref(),
                *open,
            );
            match try_ipc("web_start", payload, Duration::from_secs(6)) {
                Some(Ok(resp)) => {
                    if json_out {
                        print_json(&resp);
                        return 0;
                    }
                    match parse_web_status_from_ipc(&resp) {
                        Ok(st) => {
                            ok(&format!(
                                "Web Broadcast started on HTTP/WS :{} and WebRTC UDP :{}",
                                st.http_port, st.webrtc_udp_port
                            ));
                            info(&format!(
                                "Keep local speakers: {}",
                                if st.keep_local { "yes" } else { "no" }
                            ));
                            if st.token_required {
                                if let Some(ref tok) = st.token {
                                    info(&format!("Session token: {tok}"));
                                }
                            } else {
                                info("Authentication: disabled (--no-token)");
                            }
                            if !st.adb_reversed_serials.is_empty() {
                                ok(&format!(
                                    "ADB reverse active on: {} ({})",
                                    st.adb_reversed_serials.join(", "),
                                    st.localhost_url
                                ));
                            }
                            let prefer_adb = *adb || *open || serial.is_some();
                            if let Some(url) = primary_listen_url(&st, prefer_adb) {
                                if let Err(e) = print_qr_output(&url, &st, prefer_adb) {
                                    emit_error(&e, false);
                                }
                            }
                            0
                        }
                        Err(e) => {
                            emit_error(&e, false);
                            1
                        }
                    }
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    1
                }
                None => {
                    emit_error(DAEMON_REQUIRED_MSG, json_out);
                    1
                }
            }
        }
        Some(WebAction::Stop) => {
            match try_ipc("web_stop", serde_json::json!({}), Duration::from_secs(5)) {
                Some(Ok(resp)) => {
                    if json_out {
                        print_json(&resp);
                    } else {
                        ok("Stopped Web Broadcast.");
                    }
                    0
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    1
                }
                None => {
                    let st = controller.web_broadcast_status();
                    if json_out {
                        print_json(&serde_json::json!({
                            "ok": true,
                            "daemon_running": false,
                            "web_broadcast": st,
                        }));
                    } else {
                        info("SoundHop daemon is not running; Web Broadcast is not active.");
                    }
                    0
                }
            }
        }
        Some(WebAction::Qr { adb }) => {
            match try_ipc("web_status", serde_json::json!({}), Duration::from_secs(3)) {
                Some(Ok(resp)) => {
                    let st = match parse_web_status_from_ipc(&resp) {
                        Ok(s) => s,
                        Err(e) => {
                            emit_error(&e, json_out);
                            return 1;
                        }
                    };
                    if !st.active {
                        emit_error(
                            "Web Broadcast is not active. Start it with `soundhop web start`.",
                            json_out,
                        );
                        return 1;
                    }
                    let Some(url) = primary_listen_url(&st, *adb) else {
                        emit_error("No Web Broadcast listen URL is available.", json_out);
                        return 1;
                    };
                    if json_out {
                        match render_terminal_qr(&url) {
                            Ok(qr) => {
                                print_json(&serde_json::json!({
                                    "ok": true,
                                    "url": url,
                                    "adb": adb,
                                    "qr": qr,
                                    "web_broadcast": st,
                                }));
                                0
                            }
                            Err(e) => {
                                emit_error(&e.to_string(), true);
                                1
                            }
                        }
                    } else {
                        header("Web Broadcast QR Code");
                        if let Err(e) = print_qr_output(&url, &st, *adb) {
                            emit_error(&e, false);
                            return 1;
                        }
                        0
                    }
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    1
                }
                None => {
                    emit_error(DAEMON_REQUIRED_MSG, json_out);
                    1
                }
            }
        }
        Some(WebAction::RotateToken) => {
            match try_ipc(
                "web_rotate_token",
                serde_json::json!({}),
                Duration::from_secs(4),
            ) {
                Some(Ok(resp)) => {
                    if json_out {
                        print_json(&resp);
                        return 0;
                    }
                    match parse_web_status_from_ipc(&resp) {
                        Ok(st) => {
                            ok("Rotated Web Broadcast session token.");
                            if st.token_required {
                                if let Some(ref tok) = st.token {
                                    info(&format!("New token: {tok}"));
                                }
                            } else {
                                info("Authentication is disabled (--no-token).");
                            }
                            if let Some(url) = primary_listen_url(&st, false) {
                                info(&format!("Updated URL: {url}"));
                            }
                            0
                        }
                        Err(e) => {
                            emit_error(&e, false);
                            1
                        }
                    }
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    1
                }
                None => {
                    emit_error(DAEMON_REQUIRED_MSG, json_out);
                    1
                }
            }
        }
        Some(WebAction::Kick { client_id }) => {
            let trimmed = client_id.trim();
            if trimmed.is_empty() {
                emit_error("Specify a non-empty listener client_id to kick.", json_out);
                return 1;
            }
            match try_ipc(
                "web_kick",
                serde_json::json!({ "client_id": trimmed }),
                Duration::from_secs(4),
            ) {
                Some(Ok(resp)) => {
                    if json_out {
                        print_json(&resp);
                    } else {
                        let kicked = resp
                            .get("kicked")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if kicked {
                            ok(&format!("Disconnected listener '{trimmed}'."));
                        } else {
                            info(&format!(
                                "No active Web Broadcast listener with ID '{trimmed}'."
                            ));
                        }
                    }
                    0
                }
                Some(Err(e)) => {
                    emit_error(&e, json_out);
                    1
                }
                None => {
                    emit_error(DAEMON_REQUIRED_MSG, json_out);
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status(active: bool) -> WebBroadcastStatus {
        WebBroadcastStatus {
            active,
            http_port: if active { 4717 } else { 0 },
            webrtc_udp_port: if active { 4718 } else { 0 },
            urls: if active {
                vec!["http://192.168.1.42:4717/#t=deadbeef".to_string()]
            } else {
                Vec::new()
            },
            localhost_url: if active {
                "http://localhost:4717/#t=deadbeef".to_string()
            } else {
                String::new()
            },
            token_required: true,
            token: active.then(|| "deadbeef".to_string()),
            clients: vec![WebClientInfo {
                id: "rtc-1".to_string(),
                remote_addr: "192.168.1.99:54321".to_string(),
                transport: "webrtc".to_string(),
                user_agent: "Android Chrome".to_string(),
                connected_since: 100.0,
                bytes_sent: 1536,
                rtt_ms: Some(8.5),
                dropped_frames: 0,
            }],
            adb_reversed_serials: Vec::new(),
            keep_local: true,
            pipeline_running: active,
            capture_source: "soundhop_web_broadcast.monitor".to_string(),
        }
    }

    #[test]
    fn format_bytes_scales_cleanly() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn primary_listen_url_prefers_lan_or_adb_when_requested() {
        let st = sample_status(true);
        assert_eq!(
            primary_listen_url(&st, false).as_deref(),
            Some("http://192.168.1.42:4717/#t=deadbeef")
        );
        assert_eq!(
            primary_listen_url(&st, true).as_deref(),
            Some("http://localhost:4717/#t=deadbeef")
        );

        let inactive = sample_status(false);
        assert_eq!(primary_listen_url(&inactive, false), None);
        assert_eq!(primary_listen_url(&inactive, true), None);
    }

    #[test]
    fn build_web_start_payload_maps_flags_accurately() {
        let p1 = build_web_start_payload(Some(4800), true, false, true, false, None, false);
        assert_eq!(p1["port"], 4800);
        assert_eq!(p1["keep_local"], true);
        assert_eq!(p1["require_token"], false);
        assert!(p1.get("adb_reverse").is_none());

        let p2 = build_web_start_payload(None, false, true, false, false, Some("USB123"), true);
        assert!(p2.get("port").is_none());
        assert_eq!(p2["keep_local"], false);
        assert!(p2.get("require_token").is_none());
        assert_eq!(p2["adb_reverse"], true);
        assert_eq!(p2["serial"], "USB123");
        assert_eq!(p2["open_browser"], true);
    }

    #[test]
    fn parse_web_status_and_listener_line_round_trip() {
        let st = sample_status(true);
        let ipc_resp = serde_json::json!({
            "ok": true,
            "web_broadcast": st,
        });
        let parsed = parse_web_status_from_ipc(&ipc_resp).unwrap();
        assert!(parsed.active);
        assert_eq!(parsed.http_port, 4717);
        assert_eq!(parsed.clients.len(), 1);

        let line = format_listener_line(&parsed.clients[0]);
        assert!(line.contains("[WebRTC]"));
        assert!(line.contains("192.168.1.99:54321"));
        assert!(line.contains("8.5 ms"));
        assert!(line.contains("1.5 KiB"));

        let qr = render_terminal_qr(&parsed.urls[0]).expect("QR renders");
        assert!(qr.chars().any(|c| matches!(c, '▀' | '▄' | '█')));
    }
}
