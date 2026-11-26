//! End-to-end integration tests exercising `Daemon`, `IpcServer`, IPC client helpers
//! (`ipc::ping`, `ipc::is_running`, `ipc::request`), and `Controller` with a stateful
//! mock `ProcessRunner` over temp config/state/socket directories.

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use soundhop_core::backend::linux::LinuxAudioBackend;
use soundhop_core::config::Config;
use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::daemon::Daemon;
use soundhop_core::discovery::Peer;
use soundhop_core::ipc::{self, IpcServer};
use soundhop_core::state::State;
use soundhop_core::util::{lock_or_recover, now_secs, CommandResult, MockRunner};

#[derive(Default)]
struct MockPulseState {
    next_module_id: u32,
    default_sink: String,
    modules: Vec<(u32, String, String)>,
    sinks: Vec<(u32, String, String)>,
}

impl MockPulseState {
    fn new(default_sink: &str) -> Self {
        Self {
            next_module_id: 100,
            default_sink: default_sink.to_string(),
            modules: Vec::new(),
            sinks: vec![(1, default_sink.to_string(), "PipeWire".to_string())],
        }
    }
}

fn make_stateful_mock_runner(state: Arc<Mutex<MockPulseState>>) -> Arc<MockRunner> {
    Arc::new(MockRunner::new(move |args, _env| {
        let joined = args.join(" ");
        let mut st = state.lock().unwrap();

        if joined.ends_with(" info") {
            let stdout = format!(
                "Server Name: PulseAudio (on PipeWire 1.2.0)\n\
                 Server Version: 15.0.0\n\
                 Default Sink: {}\n\
                 Default Source: alsa_input.mock_mic\n",
                st.default_sink
            );
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout,
                stderr: String::new(),
            });
        }

        if joined.contains("list short modules") {
            let mut out = String::new();
            for (id, name, mod_args) in &st.modules {
                out.push_str(&format!("{id}\t{name}\t{mod_args}\n"));
            }
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: out,
                stderr: String::new(),
            });
        }

        if joined.contains("list short sinks") {
            let mut out = String::new();
            for (id, name, driver) in &st.sinks {
                out.push_str(&format!(
                    "{id}\t{name}\t{driver}\ts16le 2ch 48000Hz\tRUNNING\n"
                ));
            }
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: out,
                stderr: String::new(),
            });
        }

        if joined.contains("list short sources") {
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: "1\talsa_input.mock_mic\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n"
                    .to_string(),
                stderr: String::new(),
            });
        }

        if joined.contains("list short sink-inputs")
            || joined.contains("list sink-inputs")
            || joined.contains("list short source-outputs")
        {
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        if let Some(pos) = args.iter().position(|a| a == "load-module") {
            let mod_name = args.get(pos + 1).cloned().unwrap_or_default();
            let mod_args = if pos + 2 < args.len() {
                args[pos + 2..].join(" ")
            } else {
                String::new()
            };
            st.next_module_id += 1;
            let id = st.next_module_id;
            st.modules.push((id, mod_name.clone(), mod_args.clone()));

            if matches!(
                mod_name.as_str(),
                "module-tunnel-sink"
                    | "module-tunnel-sink-new"
                    | "module-null-sink"
                    | "module-combine-sink"
            ) {
                if let Some(sink_arg) = args.iter().find_map(|a| a.strip_prefix("sink_name=")) {
                    let sink_name = sink_arg.trim_matches('"').to_string();
                    st.sinks.push((id, sink_name, mod_name));
                }
            }

            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: format!("{id}\n"),
                stderr: String::new(),
            });
        }

        if let Some(pos) = args.iter().position(|a| a == "unload-module") {
            if let Some(id_str) = args.get(pos + 1) {
                if let Ok(id) = id_str.parse::<u32>() {
                    st.modules.retain(|(m_id, _, _)| *m_id != id);
                    st.sinks.retain(|(s_id, _, _)| *s_id != id);
                }
            }
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        if let Some(pos) = args.iter().position(|a| a == "set-default-sink") {
            if let Some(target) = args.get(pos + 1) {
                st.default_sink = target.clone();
            }
            return Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }

        Ok(CommandResult {
            args: args.to_vec(),
            returncode: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }))
}

#[test]
fn test_daemon_real_ipc_ping_status_config_patch_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let state_path = dir.path().join("state.json");
    let sock_path = dir.path().join("daemon.sock");

    // Pre-write a config with sharing/discovery disabled so Daemon::start runs cleanly
    // in headless CI without touching audio devices or binding LAN discovery ports.
    let mut initial_cfg = Config::default();
    initial_cfg.device.name = "IntegrationHost".to_string();
    initial_cfg.sharing.enabled = false;
    initial_cfg.discovery.udp = false;
    initial_cfg.discovery.mdns = false;
    initial_cfg.save(Some(&cfg_path)).unwrap();

    let daemon = Daemon::new(
        Some(cfg_path.clone()),
        Some(state_path),
        false,
        Some(sock_path.clone()),
    );
    daemon
        .start()
        .expect("daemon should start with temp IPC path");

    // 1. Ping & reachability
    assert!(ipc::is_reachable(Some(&sock_path)));
    assert!(ipc::is_running(Some(&sock_path)));
    let ping_info = ipc::ping(Some(&sock_path)).expect("ping should succeed");
    assert!(ping_info.pong);
    assert_eq!(ping_info.version, VERSION);
    assert_eq!(ping_info.pid, Some(u64::from(std::process::id())));

    // 2. Status over IPC
    let status = ipc::request(
        "status",
        serde_json::json!({}),
        Some(&sock_path),
        Duration::from_secs(3),
    )
    .expect("status IPC request should succeed");
    assert_eq!(status["ok"], true);
    assert_eq!(status["version"], VERSION);
    assert_eq!(status["daemon"]["running"], true);
    assert_eq!(status["device"]["name"], "IntegrationHost");
    assert_eq!(
        status["sharing"]["rtp_port"],
        soundhop_core::config::DEFAULT_RTP_PORT
    );

    // 3. Config patch over IPC + verify persistence on disk
    let patch_resp = ipc::request(
        "config_patch",
        serde_json::json!({
            "patch": {
                "device": { "name": "PatchedHost" },
                "connection": { "latency_ms": 60 },
                "sharing": { "port": 4799 }
            }
        }),
        Some(&sock_path),
        Duration::from_secs(3),
    )
    .expect("config_patch IPC request should succeed");
    assert_eq!(patch_resp["ok"], true);
    assert_eq!(patch_resp["config"]["device"]["name"], "PatchedHost");
    assert_eq!(patch_resp["config"]["connection"]["latency_ms"], 60);
    assert_eq!(patch_resp["config"]["sharing"]["port"], 4799);

    let reloaded = Config::load(Some(&cfg_path)).expect("patched config should reload from disk");
    assert_eq!(reloaded.device.name, "PatchedHost");
    assert_eq!(reloaded.connection.latency_ms, Some(60));
    assert_eq!(reloaded.sharing.port, 4799);

    // 4. Unknown command returns IPC error cleanly
    let unknown = ipc::request(
        "nonexistent_cmd",
        serde_json::json!({}),
        Some(&sock_path),
        Duration::from_secs(3),
    );
    assert!(unknown.is_err());

    // 5. Stop over IPC + clean shutdown
    let stop_resp = ipc::request(
        "stop",
        serde_json::json!({}),
        Some(&sock_path),
        Duration::from_secs(3),
    )
    .expect("stop IPC request should succeed");
    assert_eq!(stop_resp["stopping"], true);

    daemon.shutdown();
    assert!(!ipc::is_reachable(Some(&sock_path)));
}

#[test]
fn test_ipc_controller_full_lifecycle_share_connect_disconnect_and_config_patch() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let state_path = dir.path().join("state.json");
    let sock_path = dir.path().join("ctrl.sock");

    // Bind a real local TCP listener so Controller::connect's TCP reachability probe succeeds.
    let mock_peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mock_peer_port = mock_peer_listener.local_addr().unwrap().port();

    let pulse_state = Arc::new(Mutex::new(MockPulseState::new(
        "alsa_output.local_speakers",
    )));
    let runner = make_stateful_mock_runner(Arc::clone(&pulse_state));
    let backend = Arc::new(LinuxAudioBackend::with_runner(runner));

    let mut cfg = Config::default();
    cfg.device.name = "Workstation".to_string();
    cfg.sharing.enabled = false;
    cfg.sharing.port = 4725;
    cfg.connection.switch_default = true;
    cfg.connection.restore_default = true;
    cfg.save(Some(&cfg_path)).unwrap();

    let controller = Arc::new(Controller::new(
        Some(cfg),
        Some(State::default()),
        Some(backend),
        Some(cfg_path.clone()),
        Some(state_path.clone()),
        false,
    ));

    let ctrl_for_ipc = Arc::clone(&controller);
    let server = IpcServer::new(
        Arc::new(move |req: Value| {
            let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            match cmd {
                "ping" => serde_json::json!({
                    "ok": true,
                    "pong": true,
                    "pid": std::process::id(),
                    "version": VERSION,
                }),
                "status" => {
                    let mut st = ctrl_for_ipc.status(None);
                    if let Some(obj) = st.as_object_mut() {
                        obj.insert("ok".to_string(), Value::Bool(true));
                    }
                    st
                }
                "share" => {
                    let enabled = req.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                    match ctrl_for_ipc.share(enabled, Some(false)) {
                        Ok(mut res) => {
                            if let Some(obj) = res.as_object_mut() {
                                obj.entry("ok".to_string()).or_insert(Value::Bool(true));
                            }
                            res
                        }
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    }
                }
                "connect" => {
                    let host = req
                        .get("host")
                        .and_then(|v| v.as_str())
                        .unwrap_or("127.0.0.1");
                    let port = req.get("port").and_then(|v| v.as_u64()).unwrap_or(4713) as u16;
                    let name = req
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("LivingRoom");
                    let peer = Peer {
                        id: format!("peer-{host}-{port}"),
                        name: name.to_string(),
                        host: host.to_string(),
                        port,
                        version: VERSION.to_string(),
                        protocol: PROTOCOL_VERSION,
                        security: "trusted-lan".to_string(),
                        platform: "linux".to_string(),
                        last_seen: now_secs(),
                        source: "manual".to_string(),
                        rtt_ms: None,
                    };
                    match ctrl_for_ipc.connect(&peer, ConnectOptions::default()) {
                        Ok(conn) => serde_json::json!({ "ok": true, "connection": conn }),
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    }
                }
                "disconnect" => {
                    match ctrl_for_ipc.disconnect(false, Some(true), true, Some(false), true, false)
                    {
                        Ok(mut res) => {
                            if let Some(obj) = res.as_object_mut() {
                                obj.entry("ok".to_string()).or_insert(Value::Bool(true));
                            }
                            res
                        }
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    }
                }
                "config_patch" => {
                    if let Some(patch) = req.get("patch") {
                        if let Err(e) = lock_or_recover(&ctrl_for_ipc.config).apply_patch(patch) {
                            return serde_json::json!({ "ok": false, "error": e.to_string() });
                        }
                        ctrl_for_ipc.save();
                    }
                    let cfg_json = lock_or_recover(&ctrl_for_ipc.config).to_json_value();
                    serde_json::json!({ "ok": true, "config": cfg_json })
                }
                _ => serde_json::json!({ "ok": false, "error": format!("Unknown cmd: {cmd}") }),
            }
        }),
        Some(sock_path.clone()),
    );
    server.start().expect("IpcServer should start");

    let timeout = Duration::from_secs(3);

    // 1. Ping
    let ping_info = ipc::ping(Some(&sock_path)).expect("ping should succeed");
    assert!(ping_info.pong);
    assert_eq!(ping_info.version, VERSION);

    // 2. Initial status
    let st0 = ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert_eq!(st0["backend"]["available"], true);
    assert_eq!(st0["sharing"]["enabled"], false);
    assert_eq!(st0["sharing"]["listener_module"], 0);
    assert!(st0["connection"].is_null());

    // 3. Share ON
    let share_on = ipc::request(
        "share",
        serde_json::json!({ "enabled": true }),
        Some(&sock_path),
        timeout,
    )
    .expect("share on should succeed");
    assert_eq!(share_on["ok"], true);
    let st_share_on =
        ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert_eq!(st_share_on["sharing"]["enabled"], true);
    assert!(
        st_share_on["sharing"]["listener_module"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(
        pulse_state
            .lock()
            .unwrap()
            .modules
            .iter()
            .any(|(_, name, _)| name == "module-native-protocol-tcp"),
        "TCP listener module should be loaded when sharing is enabled"
    );

    // 4. Share OFF
    let share_off = ipc::request(
        "share",
        serde_json::json!({ "enabled": false }),
        Some(&sock_path),
        timeout,
    )
    .expect("share off should succeed");
    assert_eq!(share_off["ok"], true);
    let st_share_off =
        ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert_eq!(st_share_off["sharing"]["enabled"], false);
    assert_eq!(st_share_off["sharing"]["listener_module"], 0);
    assert!(
        !pulse_state
            .lock()
            .unwrap()
            .modules
            .iter()
            .any(|(_, name, _)| name == "module-native-protocol-tcp"),
        "TCP listener module should be unloaded when sharing is disabled"
    );

    // 5. Connect to reachable peer
    let conn_resp = ipc::request(
        "connect",
        serde_json::json!({
            "host": "127.0.0.1",
            "port": mock_peer_port,
            "name": "LivingRoom"
        }),
        Some(&sock_path),
        timeout,
    )
    .expect("connect should succeed against mock peer");
    assert_eq!(conn_resp["ok"], true);
    assert_eq!(conn_resp["connection"]["connected"], true);
    assert_eq!(conn_resp["connection"]["name"], "LivingRoom");
    assert_eq!(conn_resp["connection"]["host"], "127.0.0.1");
    assert_eq!(conn_resp["connection"]["port"], mock_peer_port);

    let st_connected =
        ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert_eq!(st_connected["connection"]["connected"], true);
    assert_ne!(
        pulse_state.lock().unwrap().default_sink,
        "alsa_output.local_speakers",
        "default sink should switch to remote tunnel sink"
    );

    // 6. Disconnect and verify default sink restored
    let disc_resp = ipc::request(
        "disconnect",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .expect("disconnect should succeed");
    assert_eq!(disc_resp["ok"], true);
    let st_disconnected =
        ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert!(st_disconnected["connection"].is_null());
    assert_eq!(
        pulse_state.lock().unwrap().default_sink,
        "alsa_output.local_speakers",
        "default sink should be restored to local speakers on disconnect"
    );

    // 7. Config patch over IPC
    let patch_resp = ipc::request(
        "config_patch",
        serde_json::json!({
            "patch": {
                "device": { "name": "StudioPC" },
                "connection": { "auto_connect": true, "latency_ms": 35 }
            }
        }),
        Some(&sock_path),
        timeout,
    )
    .expect("config_patch should succeed");
    assert_eq!(patch_resp["config"]["device"]["name"], "StudioPC");
    assert_eq!(patch_resp["config"]["connection"]["auto_connect"], true);
    assert_eq!(patch_resp["config"]["connection"]["latency_ms"], 35);

    let reloaded = Config::load(Some(&cfg_path)).unwrap();
    assert_eq!(reloaded.device.name, "StudioPC");
    assert!(reloaded.connection.auto_connect);
    assert_eq!(reloaded.connection.latency_ms, Some(35));

    server.stop();
    drop(mock_peer_listener);
}

#[test]
fn test_daemon_web_broadcast_ipc_roundtrip_and_peer_coexistence() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let state_path = dir.path().join("state.json");
    let sock_path = dir.path().join("web-daemon.sock");

    let pulse_state = Arc::new(Mutex::new(MockPulseState::new(
        "alsa_output.local_speakers",
    )));
    let runner = make_stateful_mock_runner(Arc::clone(&pulse_state));
    let backend = Arc::new(LinuxAudioBackend::with_runner(runner));

    let mut cfg = Config::default();
    cfg.device.name = "WebBroadcastHost".to_string();
    cfg.sharing.enabled = false;
    cfg.sharing.web_enabled = false;
    cfg.sharing.web_port = 0;
    cfg.sharing.webrtc_port = 0;
    cfg.discovery.udp = false;
    cfg.discovery.mdns = false;
    cfg.connection.switch_default = true;
    cfg.connection.restore_default = true;
    cfg.save(Some(&cfg_path)).unwrap();

    let controller = Arc::new(Controller::new(
        Some(cfg),
        Some(State::default()),
        Some(backend),
        Some(cfg_path),
        Some(state_path),
        false,
    ));

    let daemon = Daemon::with_controller(Arc::clone(&controller), Some(sock_path.clone()));
    daemon
        .start()
        .expect("daemon with mock backend should start cleanly");

    let timeout = Duration::from_secs(3);

    // 1. Initial web_status / broadcast_status is inactive
    let st0 = ipc::request(
        "web_status",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .expect("web_status should succeed");
    assert_eq!(st0["ok"], true);
    assert_eq!(st0["web_broadcast"]["active"], false);

    // 2. web_start binds HTTP + WebRTC ports, generates ephemeral token, and sets up Linux sinks
    let started = ipc::request(
        "web_start",
        serde_json::json!({
            "port": 0,
            "keep_local": true,
            "require_token": true
        }),
        Some(&sock_path),
        timeout,
    )
    .expect("web_start should succeed");
    assert_eq!(started["ok"], true);
    assert_eq!(started["web_broadcast"]["active"], true);
    assert_eq!(started["web_broadcast"]["keep_local"], true);
    assert_eq!(started["web_broadcast"]["token_required"], true);
    let http_port = started["web_broadcast"]["http_port"].as_u64().unwrap();
    assert!(http_port > 0);
    assert!(
        started["web_broadcast"]["webrtc_udp_port"]
            .as_u64()
            .unwrap()
            > 0
    );
    let tok1 = started["web_broadcast"]["token"]
        .as_str()
        .expect("token should be present")
        .to_string();
    assert_eq!(tok1.len(), 32);
    assert!(started["web_broadcast"]["localhost_url"]
        .as_str()
        .unwrap()
        .contains(&format!("#t={tok1}")));

    #[cfg(target_os = "linux")]
    {
        let ps = pulse_state.lock().unwrap();
        assert!(ps
            .sinks
            .iter()
            .any(|(_, name, _)| name == soundhop_core::constants::WEB_BROADCAST_SINK_NAME));
        assert!(ps
            .sinks
            .iter()
            .any(|(_, name, _)| name == soundhop_core::constants::MULTIROOM_SINK_NAME));
        assert_eq!(
            ps.default_sink,
            soundhop_core::constants::MULTIROOM_SINK_NAME
        );
    }

    // 3. status exposes web_broadcast while keeping connection == null in standalone mode
    let full_status = ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout)
        .expect("status should succeed");
    assert_eq!(full_status["ok"], true);
    assert_eq!(full_status["web_broadcast"]["active"], true);
    assert!(full_status["connection"].is_null());

    // 4. broadcast_status alias matches web_status
    let b_status = ipc::request(
        "broadcast_status",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .expect("broadcast_status should succeed");
    assert_eq!(b_status["ok"], true);
    assert_eq!(b_status["web_broadcast"]["active"], true);
    assert_eq!(b_status["web_broadcast"]["http_port"], http_port);

    // 5. web_rotate_token issues a fresh 32-char token and updates URLs
    let rotated = ipc::request(
        "web_rotate_token",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .expect("web_rotate_token should succeed");
    assert_eq!(rotated["ok"], true);
    let tok2 = rotated["token"].as_str().unwrap().to_string();
    assert_eq!(tok2.len(), 32);
    assert_ne!(tok1, tok2);
    assert_eq!(rotated["web_broadcast"]["token"], tok2);

    // 6. web_kick with unknown client returns ok: true, kicked: false
    let kick_resp = ipc::request(
        "web_kick",
        serde_json::json!({ "client_id": "nonexistent" }),
        Some(&sock_path),
        timeout,
    )
    .expect("web_kick should succeed");
    assert_eq!(kick_resp["ok"], true);
    assert_eq!(kick_resp["kicked"], false);

    // 7. disconnect IPC is a no-op while standalone Web Broadcast is active
    let disc_resp = ipc::request(
        "disconnect",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .expect("disconnect should return cleanly");
    assert_eq!(disc_resp["disconnected"], false);
    let st_after_disc = ipc::request(
        "web_status",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .unwrap();
    assert_eq!(st_after_disc["web_broadcast"]["active"], true);

    // 8. config_patch changing web_require_token rebinds active Web Broadcast
    let patched = ipc::request(
        "config_patch",
        serde_json::json!({
            "patch": {
                "sharing": { "web_require_token": false }
            }
        }),
        Some(&sock_path),
        timeout,
    )
    .expect("config_patch should succeed");
    assert_eq!(patched["ok"], true);
    let st_after_patch = ipc::request(
        "web_status",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .unwrap();
    assert_eq!(st_after_patch["web_broadcast"]["active"], true);
    assert_eq!(st_after_patch["web_broadcast"]["token_required"], false);
    assert!(st_after_patch["web_broadcast"]["token"].is_null());

    // 9. web_stop stops servers and restores default sink
    let stopped = ipc::request("web_stop", serde_json::json!({}), Some(&sock_path), timeout)
        .expect("web_stop should succeed");
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["web_broadcast"]["active"], false);
    assert!(stopped["web_broadcast"]["token"].is_null());
    #[cfg(target_os = "linux")]
    assert_eq!(
        pulse_state.lock().unwrap().default_sink,
        "alsa_output.local_speakers"
    );

    // 10. Simultaneous peer session coexistence via broadcast_start / broadcast_stop aliases
    let mock_peer_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mock_peer_port = mock_peer_listener.local_addr().unwrap().port();
    let conn_resp = ipc::request(
        "connect",
        serde_json::json!({
            "host": "127.0.0.1",
            "port": mock_peer_port,
            "name": "StudioPeer"
        }),
        Some(&sock_path),
        timeout,
    )
    .expect("connect should succeed");
    assert_eq!(conn_resp["ok"], true);

    let b_started = ipc::request(
        "broadcast_start",
        serde_json::json!({ "port": 0 }),
        Some(&sock_path),
        timeout,
    )
    .expect("broadcast_start should succeed alongside active peer");
    assert_eq!(b_started["ok"], true);
    assert_eq!(b_started["web_broadcast"]["active"], true);

    let st_both = ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert_eq!(st_both["connection"]["connected"], true);
    assert_eq!(st_both["connection"]["name"], "StudioPeer");
    assert_eq!(st_both["web_broadcast"]["active"], true);

    let b_stopped = ipc::request(
        "broadcast_stop",
        serde_json::json!({}),
        Some(&sock_path),
        timeout,
    )
    .expect("broadcast_stop should succeed");
    assert_eq!(b_stopped["ok"], true);
    assert_eq!(b_stopped["web_broadcast"]["active"], false);

    let st_peer_intact =
        ipc::request("status", serde_json::json!({}), Some(&sock_path), timeout).unwrap();
    assert_eq!(st_peer_intact["connection"]["connected"], true);
    assert_eq!(st_peer_intact["connection"]["name"], "StudioPeer");

    daemon.shutdown();
    drop(mock_peer_listener);
    assert!(!ipc::is_reachable(Some(&sock_path)));
}
