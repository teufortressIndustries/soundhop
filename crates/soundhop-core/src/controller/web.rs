//! Controller orchestration for Web Broadcast (HTTP + WebRTC + WS fallback) and USB ADB reverse tunnels.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::constants::{MULTIROOM_SINK_NAME, WEB_BROADCAST_SINK_NAME};
use crate::transport::{AudioParams, CaptureEndpoint, CodecId};
use crate::util::{lock_or_recover, now_secs};
#[cfg(test)]
use crate::web::WebError;
use crate::web::{
    format_listen_url, CaptureFactory, WebBroadcastConfig, WebBroadcastStatus, WebCaptureTarget,
    WebClientInfo,
};

use super::session::parse_audio_profile;
use super::{Controller, ControllerError};

const WEB_BROADCAST_PEER_ID: &str = "web-broadcast";

impl Controller {
    /// Start Web Broadcast HTTP/WS/WebRTC servers and configure platform audio capture.
    pub fn start_web_broadcast(
        &self,
        port: Option<u16>,
        keep_local: Option<bool>,
        require_token: Option<bool>,
    ) -> Result<WebBroadcastStatus, ControllerError> {
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.start_web_broadcast_internal(port, keep_local, require_token)
    }

    pub(super) fn start_web_broadcast_internal(
        &self,
        port: Option<u16>,
        keep_local: Option<bool>,
        require_token: Option<bool>,
    ) -> Result<WebBroadcastStatus, ControllerError> {
        if self.web_broadcast.is_active() {
            return Err(ControllerError::Message(
                "Web Broadcast is already running".to_string(),
            ));
        }

        let cfg = lock_or_recover(&self.config).clone();
        let st_snap = lock_or_recover(&self.state).clone();
        let eff_keep_local = keep_local.unwrap_or(cfg.sharing.web_keep_local);
        let eff_require_token = require_token.unwrap_or(cfg.sharing.web_require_token);

        let profile = parse_audio_profile(&cfg.audio.profile);
        let audio_params = AudioParams {
            codec: CodecId::Opus,
            sample_rate: 48_000,
            channels: 2,
            frame_ms: 20,
            bitrate_kbps: profile.bitrate_kbps().min(510),
            jitter_ms: 40,
        };

        let mut web_cfg = WebBroadcastConfig::from_app_config(&cfg, audio_params, "");
        if let Some(p) = port {
            web_cfg.http_port = p;
        }
        web_cfg.keep_local = eff_keep_local;
        web_cfg.require_token = eff_require_token;

        let mut web_null_module = 0u32;
        let mut combine_module = 0u32;
        let mut standalone_linux_sink: Option<String> = None;

        let (capture_endpoint, capture_label) = if cfg!(target_os = "windows") {
            (
                CaptureEndpoint::SystemWasapiLoopback,
                "Windows Default Output (WASAPI Loopback)".to_string(),
            )
        } else if cfg!(target_os = "linux") {
            self.ensure_backend()?;
            if st_snap.is_connected() && st_snap.connected_peer != WEB_BROADCAST_PEER_ID {
                // Coexist with an active peer/multi-room session without touching its state or modules.
                let monitor_dev = format!("{}.monitor", st_snap.connected_sink);
                let label = monitor_dev.clone();
                (
                    CaptureEndpoint::SystemPulseMonitor {
                        device: monitor_dev,
                    },
                    label,
                )
            } else {
                // Clean up any leftover null-sink module with the same name before creating a fresh one.
                if let Ok(mods) = self.backend.modules() {
                    for m in mods {
                        if m.name == "module-null-sink"
                            && m.arg("sink_name").as_deref() == Some(WEB_BROADCAST_SINK_NAME)
                        {
                            let _ = self.backend.unload_module(m.id, true);
                        }
                    }
                }

                self.remember_previous_default();
                let null_id = self
                    .backend
                    .load_module(
                        "module-null-sink",
                        &[
                            format!("sink_name={WEB_BROADCAST_SINK_NAME}"),
                            "sink_properties=device.description=SoundHop_Web_Broadcast".to_string(),
                            "rate=48000".to_string(),
                            "channels=2".to_string(),
                        ],
                    )
                    .map_err(|e| {
                        ControllerError::Message(format!(
                            "Could not create Web Broadcast virtual sink {WEB_BROADCAST_SINK_NAME}: {e}"
                        ))
                    })?;
                web_null_module = null_id;

                let target_sink = if eff_keep_local {
                    let prev = lock_or_recover(&self.state).previous_default_sink.clone();
                    let local_sink = self.local_default_candidate(&prev);
                    let mut slaves = Vec::new();
                    if !local_sink.is_empty() {
                        slaves.push(local_sink);
                    }
                    slaves.push(WEB_BROADCAST_SINK_NAME.to_string());
                    match self.backend.create_combined_sink(
                        MULTIROOM_SINK_NAME,
                        "WebBroadcast+Local",
                        &slaves,
                    ) {
                        Ok(cid) => {
                            combine_module = cid;
                            MULTIROOM_SINK_NAME.to_string()
                        }
                        Err(e) => {
                            let _ = self.backend.unload_module(null_id, true);
                            return Err(ControllerError::Message(format!(
                                "Could not create combined sink for Web Broadcast: {e}"
                            )));
                        }
                    }
                } else {
                    WEB_BROADCAST_SINK_NAME.to_string()
                };

                standalone_linux_sink = Some(target_sink);
                let monitor_dev = format!("{WEB_BROADCAST_SINK_NAME}.monitor");
                let label = monitor_dev.clone();
                (
                    CaptureEndpoint::SystemPulseMonitor {
                        device: monitor_dev,
                    },
                    label,
                )
            }
        } else {
            return Err(ControllerError::Message(
                "Web Broadcast capture is not supported on this platform".to_string(),
            ));
        };

        web_cfg.capture_source_label = capture_label.clone();

        #[cfg(test)]
        let test_override = lock_or_recover(&self.capture_factory_override).clone();

        let capture_factory: CaptureFactory = Arc::new(move || {
            #[cfg(test)]
            if let Some(ref override_fn) = test_override {
                let endpoint = override_fn().map_err(WebError::Transport)?;
                return Ok(WebCaptureTarget {
                    endpoint,
                    audio_params,
                    label: capture_label.clone(),
                });
            }
            Ok(WebCaptureTarget {
                endpoint: capture_endpoint.clone(),
                audio_params,
                label: capture_label.clone(),
            })
        });

        let status = match self.web_broadcast.start(web_cfg, capture_factory) {
            Ok(st) => st,
            Err(e) => {
                if combine_module > 0 {
                    let _ = self.backend.unload_module(combine_module, true);
                }
                if web_null_module > 0 {
                    let _ = self.backend.unload_module(web_null_module, true);
                }
                self.log_line("ERROR", &format!("Web Broadcast start failed: {e}"));
                return Err(ControllerError::Message(e.to_string()));
            }
        };

        if let Some(ref target_sink) = standalone_linux_sink {
            {
                let mut st = lock_or_recover(&self.state);
                st.connected_peer = WEB_BROADCAST_PEER_ID.to_string();
                st.connected_name = if eff_keep_local {
                    "Web Broadcast + Local".to_string()
                } else {
                    "Web Broadcast".to_string()
                };
                st.connected_host.clear();
                st.connected_port = 0;
                st.connected_sink = target_sink.clone();
                st.connected_module = web_null_module;
                st.multiroom_peers.clear();
                st.combine_module = combine_module;
                st.keep_local = eff_keep_local;
                st.connected_since = now_secs();
                st.connected_by = WEB_BROADCAST_PEER_ID.to_string();
                st.auto_paused = false;
                st.web_broadcast_active = true;
                st.web_port = status.http_port;
                st.web_null_module = web_null_module;
            }
            self.save();

            let selective_default = cfg.routing.default_mode == "selective";
            if cfg.connection.switch_default && !selective_default {
                let _ = self.make_default(target_sink, cfg.connection.move_streams);
            }
            let _ = self.enforce_routed_apps();
        } else {
            // Windows or simultaneous peer session: do not overwrite connected_peer / connected_sink.
            {
                let mut st = lock_or_recover(&self.state);
                st.web_broadcast_active = true;
                st.web_port = status.http_port;
                st.web_null_module = 0;
            }
            self.save();
        }

        self.log_line(
            "INFO",
            &format!(
                "Web Broadcast started on port {} (keep_local={}, require_token={})",
                status.http_port, eff_keep_local, eff_require_token
            ),
        );

        Ok(self.web_broadcast_status())
    }

    /// Stop Web Broadcast, tear down any tracked USB `adb reverse` tunnels, and restore Linux sinks.
    pub fn stop_web_broadcast(&self) -> Result<WebBroadcastStatus, ControllerError> {
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.stop_web_broadcast_internal()
    }

    pub(super) fn stop_web_broadcast_internal(
        &self,
    ) -> Result<WebBroadcastStatus, ControllerError> {
        self.remove_all_web_adb_reverse_tunnels();
        let _ = self.web_broadcast.stop();

        let st_snap = lock_or_recover(&self.state).clone();
        if st_snap.connected_peer == WEB_BROADCAST_PEER_ID {
            if st_snap.combine_module > 0 {
                let _ = self.backend.unload_module(st_snap.combine_module, true);
            }
            if st_snap.web_null_module > 0 {
                let _ = self.backend.unload_module(st_snap.web_null_module, true);
            }
            if st_snap.connected_module > 0
                && st_snap.connected_module != st_snap.web_null_module
                && st_snap.connected_module != st_snap.combine_module
            {
                let _ = self.backend.unload_module(st_snap.connected_module, true);
            }

            let cfg = lock_or_recover(&self.config).clone();
            if cfg.connection.restore_default {
                let cand = self.local_default_candidate(&st_snap.previous_default_sink);
                if !cand.is_empty()
                    && self.backend.set_default_sink(&cand).is_ok()
                    && cfg.connection.move_streams
                {
                    let _ = self.backend.move_sink_inputs(&cand, None);
                }
            }

            {
                let mut st = lock_or_recover(&self.state);
                st.clear_connection();
                st.previous_default_sink.clear();
                st.previous_default_source.clear();
                st.clear_web_broadcast();
            }
        } else {
            if st_snap.web_null_module > 0
                && st_snap.web_null_module != st_snap.connected_module
                && st_snap.web_null_module != st_snap.combine_module
            {
                let _ = self.backend.unload_module(st_snap.web_null_module, true);
            }
            lock_or_recover(&self.state).clear_web_broadcast();
        }
        self.save();

        if st_snap.web_broadcast_active || st_snap.connected_peer == WEB_BROADCAST_PEER_ID {
            self.log_line("INFO", "Web Broadcast stopped");
        }

        Ok(self.web_broadcast_status())
    }

    /// Rotate the ephemeral Web Broadcast session token in place.
    pub fn rotate_web_token(&self) -> Result<WebBroadcastStatus, ControllerError> {
        self.web_broadcast
            .rotate_token()
            .map_err(|e| ControllerError::Message(e.to_string()))
    }

    /// Disconnect a single WebRTC or WebSocket browser listener by ID.
    pub fn kick_web_client(&self, client_id: &str) -> Result<bool, ControllerError> {
        self.web_broadcast
            .kick_client(client_id)
            .map_err(|e| ControllerError::Message(e.to_string()))
    }

    /// Return the current live Web Broadcast status snapshot.
    pub fn web_broadcast_status(&self) -> WebBroadcastStatus {
        self.web_broadcast.status()
    }

    /// List all currently connected browser listeners (WebRTC + WebSocket).
    pub fn list_web_clients(&self) -> Vec<WebClientInfo> {
        self.web_broadcast.list_clients()
    }

    /// Enable or disable a USB `adb reverse` tunnel for Web Broadcast and optionally launch the phone browser.
    pub fn web_adb_reverse(
        &self,
        serial: Option<&str>,
        port: Option<u16>,
        enable: bool,
        open_browser: bool,
    ) -> Result<WebBroadcastStatus, ControllerError> {
        let requested_serial = serial.map(str::trim).filter(|s| !s.is_empty());
        let active_status = self.web_broadcast.status();
        let target_port = port
            .filter(|&p| p > 0)
            .or_else(|| active_status.active.then_some(active_status.http_port))
            .filter(|&p| p > 0)
            .unwrap_or_else(|| lock_or_recover(&self.config).sharing.web_port);

        if target_port == 0 {
            return Err(ControllerError::Message(
                "Web Broadcast port must be non-zero".to_string(),
            ));
        }

        if enable {
            let target_serial = match requested_serial {
                Some(s) => s.to_string(),
                None => self.pick_ready_adb_serial().ok_or_else(|| {
                    ControllerError::Message(
                        "No ready Android ADB device found for adb reverse".to_string(),
                    )
                })?,
            };
            self.android_manager
                .setup_reverse_tunnel(&target_serial, target_port, target_port)
                .map_err(|e| ControllerError::Message(e.to_string()))?;

            let serials = {
                let mut st = lock_or_recover(&self.state);
                if !st.web_adb_reverse_serials.contains(&target_serial) {
                    st.web_adb_reverse_serials.push(target_serial.clone());
                }
                st.web_adb_reverse_serials.clone()
            };
            self.web_broadcast.set_adb_reversed_serials(serials);
            self.save();

            if open_browser {
                let url = if active_status.active && !active_status.localhost_url.is_empty() {
                    active_status.localhost_url
                } else {
                    format_listen_url("localhost", target_port, active_status.token.as_deref())
                };
                self.android_manager
                    .open_browser_url(&target_serial, &url)
                    .map_err(|e| ControllerError::Message(e.to_string()))?;
            }
        } else {
            let targets: Vec<String> = if let Some(s) = requested_serial {
                vec![s.to_string()]
            } else {
                let mut set: BTreeSet<String> = lock_or_recover(&self.state)
                    .web_adb_reverse_serials
                    .iter()
                    .cloned()
                    .collect();
                for (s, _) in self.android_manager.tracked_reverse_tunnels() {
                    set.insert(s);
                }
                if set.is_empty() {
                    if let Some(s) = self.pick_ready_adb_serial() {
                        set.insert(s);
                    }
                }
                set.into_iter().collect()
            };

            let remove_port = port.filter(|&p| p > 0);
            for target_serial in &targets {
                self.android_manager
                    .remove_reverse_tunnel(target_serial, remove_port)
                    .map_err(|e| ControllerError::Message(e.to_string()))?;
            }

            let serials = {
                let mut st = lock_or_recover(&self.state);
                if requested_serial.is_some() {
                    st.web_adb_reverse_serials.retain(|s| !targets.contains(s));
                } else {
                    st.web_adb_reverse_serials.clear();
                }
                st.web_adb_reverse_serials.clone()
            };
            self.web_broadcast.set_adb_reversed_serials(serials);
            self.save();
        }

        Ok(self.web_broadcast_status())
    }

    fn pick_ready_adb_serial(&self) -> Option<String> {
        let devices = self.android_manager.list_devices();
        devices
            .iter()
            .find(|d| !d.is_wifi && d.is_ready())
            .or_else(|| devices.iter().find(|d| d.is_ready()))
            .map(|d| d.serial.clone())
    }

    pub(crate) fn remove_all_web_adb_reverse_tunnels(&self) {
        let mut serials: BTreeSet<String> = lock_or_recover(&self.state)
            .web_adb_reverse_serials
            .iter()
            .cloned()
            .collect();
        for (s, _) in self.android_manager.tracked_reverse_tunnels() {
            serials.insert(s);
        }
        for serial in serials {
            let _ = self.android_manager.remove_reverse_tunnel(&serial, None);
        }
        self.web_broadcast.set_adb_reversed_serials(Vec::new());
    }

    /// Remove any leftover `web_adb_reverse_serials` from an unclean prior shutdown.
    pub(crate) fn cleanup_stale_web_adb_reverse(&self) {
        let stale = lock_or_recover(&self.state).web_adb_reverse_serials.clone();
        if stale.is_empty() && self.android_manager.tracked_reverse_tunnels().is_empty() {
            return;
        }
        self.remove_all_web_adb_reverse_tunnels();
        {
            let mut st = lock_or_recover(&self.state);
            st.web_adb_reverse_serials.clear();
            if !self.web_broadcast.is_active() {
                st.web_broadcast_active = false;
                st.web_port = 0;
            }
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::backend::linux::LinuxAudioBackend;
    use crate::config::Config;
    use crate::constants::{MULTIROOM_SINK_NAME, WEB_BROADCAST_SINK_NAME};
    use crate::controller::Controller;
    use crate::state::State;
    use crate::util::{CommandResult, MockRunner};

    type SinkList = Arc<Mutex<Vec<(u32, String)>>>;

    struct MockLinuxCtrl {
        ctrl: Controller,
        unloaded: Arc<Mutex<Vec<u32>>>,
        default_sink: Arc<Mutex<String>>,
    }

    fn make_linux_controller(runner_state: Option<SinkList>) -> MockLinuxCtrl {
        let next_id = Arc::new(AtomicU32::new(200));
        let sinks = runner_state.unwrap_or_else(|| Arc::new(Mutex::new(Vec::new())));
        let unloaded = Arc::new(Mutex::new(Vec::<u32>::new()));
        let default_sink = Arc::new(Mutex::new("alsa_output.pci".to_string()));

        let next_c = Arc::clone(&next_id);
        let sinks_c = Arc::clone(&sinks);
        let unloaded_c = Arc::clone(&unloaded);
        let def_c = Arc::clone(&default_sink);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            let stdout = if joined.ends_with(" info") {
                format!(
                    "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: {}\n",
                    *def_c.lock().unwrap()
                )
            } else if joined.contains("set-default-sink") {
                if let Some(target) = args.last() {
                    *def_c.lock().unwrap() = target.clone();
                }
                String::new()
            } else if args.iter().any(|a| a == "unload-module") {
                if let Some(id) = args.last().and_then(|s| s.parse::<u32>().ok()) {
                    unloaded_c.lock().unwrap().push(id);
                    sinks_c.lock().unwrap().retain(|(mid, _)| *mid != id);
                }
                String::new()
            } else if args.iter().any(|a| a == "load-module") {
                let id = next_c.fetch_add(1, Ordering::SeqCst);
                if let Some(sname) = args.iter().find_map(|a| a.strip_prefix("sink_name=")) {
                    sinks_c.lock().unwrap().push((id, sname.to_string()));
                }
                format!("{id}\n")
            } else if joined.contains("list short sinks") {
                let mut out =
                    "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n".to_string();
                for (id, sname) in sinks_c.lock().unwrap().iter() {
                    out.push_str(&format!(
                        "{id}\t{sname}\tmodule-null-sink\ts16le 2ch 48000Hz\tRUNNING\n"
                    ));
                }
                out
            } else if joined.contains("list short modules") {
                let mut out = String::new();
                for (id, sname) in sinks_c.lock().unwrap().iter() {
                    let mod_name = if sname == MULTIROOM_SINK_NAME {
                        "module-combine-sink"
                    } else {
                        "module-null-sink"
                    };
                    out.push_str(&format!("{id}\t{mod_name}\tsink_name={sname}\n"));
                }
                out
            } else {
                String::new()
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout,
                stderr: String::new(),
            })
        }));

        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let mut cfg = Config::default();
        cfg.sharing.web_port = 0;
        cfg.sharing.webrtc_port = 0;
        let ctrl = Controller::new(
            Some(cfg),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        );
        ctrl.set_save_enabled(false);
        MockLinuxCtrl {
            ctrl,
            unloaded,
            default_sink,
        }
    }

    #[test]
    fn standalone_web_broadcast_keep_local_and_disconnect_noop_and_reconcile() {
        let sinks = Arc::new(Mutex::new(Vec::new()));
        let MockLinuxCtrl {
            ctrl,
            unloaded,
            default_sink,
        } = make_linux_controller(Some(Arc::clone(&sinks)));

        let st = ctrl
            .start_web_broadcast(Some(0), Some(true), Some(true))
            .expect("start_web_broadcast succeeds");
        assert!(st.active);
        assert!(st.http_port > 0);
        assert!(st.token.is_some());
        assert!(st.keep_local);

        #[cfg(target_os = "linux")]
        {
            assert_eq!(ctrl.state.lock().unwrap().connected_peer, "web-broadcast");
            assert_eq!(
                ctrl.state.lock().unwrap().connected_sink,
                MULTIROOM_SINK_NAME
            );
            assert_eq!(*default_sink.lock().unwrap(), MULTIROOM_SINK_NAME);
            assert!(sinks
                .lock()
                .unwrap()
                .iter()
                .any(|(_, n)| n == WEB_BROADCAST_SINK_NAME));
            assert!(sinks
                .lock()
                .unwrap()
                .iter()
                .any(|(_, n)| n == MULTIROOM_SINK_NAME));
        }

        // status()["connection"] must remain Null while web_broadcast is populated.
        let full_status = ctrl.status(None);
        assert!(full_status["connection"].is_null());
        assert_eq!(full_status["web_broadcast"]["active"], true);

        // Peer disconnect() must be a no-op while Web Broadcast is active.
        let disc = ctrl
            .disconnect(false, Some(true), false, Some(false), false, false)
            .expect("disconnect returns ok no-op");
        assert_eq!(disc["disconnected"], false);
        assert!(ctrl.web_broadcast.is_active());

        // Stop Web Broadcast cleanly tears down virtual sinks and restores default sink.
        let stopped = ctrl.stop_web_broadcast().expect("stop succeeds");
        assert!(!stopped.active);
        assert!(!ctrl.state.lock().unwrap().is_connected());
        #[cfg(target_os = "linux")]
        {
            assert!(!unloaded.lock().unwrap().is_empty());
            assert_eq!(*default_sink.lock().unwrap(), "alsa_output.pci");
        }
        let _ = unloaded;
        let _ = default_sink;
    }

    #[test]
    fn reconcile_clears_stale_web_broadcast_state() {
        let sinks = Arc::new(Mutex::new(vec![
            (201, WEB_BROADCAST_SINK_NAME.to_string()),
            (202, MULTIROOM_SINK_NAME.to_string()),
        ]));
        let MockLinuxCtrl {
            ctrl,
            unloaded,
            default_sink,
        } = make_linux_controller(Some(sinks));
        {
            let mut st = ctrl.state.lock().unwrap();
            st.connected_peer = "web-broadcast".to_string();
            st.connected_sink = MULTIROOM_SINK_NAME.to_string();
            st.connected_module = 201;
            st.web_null_module = 201;
            st.combine_module = 202;
            st.web_broadcast_active = true;
            st.previous_default_sink = "alsa_output.pci".to_string();
        }
        *default_sink.lock().unwrap() = MULTIROOM_SINK_NAME.to_string();

        let rec = ctrl.reconcile();
        let actions = rec["actions"].as_array().unwrap();
        assert!(actions
            .iter()
            .any(|a| a.as_str().is_some_and(|s| s.contains("web-broadcast"))));
        assert!(!ctrl.state.lock().unwrap().is_connected());
        assert!(!ctrl.state.lock().unwrap().web_broadcast_active);
        assert!(unloaded.lock().unwrap().contains(&201));
        assert!(unloaded.lock().unwrap().contains(&202));
        assert_eq!(*default_sink.lock().unwrap(), "alsa_output.pci");
    }

    #[test]
    fn simultaneous_peer_session_coexists_with_web_broadcast() {
        let sinks = Arc::new(Mutex::new(vec![(150, "soundhop_peer_1".to_string())]));
        let MockLinuxCtrl {
            ctrl,
            unloaded,
            default_sink: _,
        } = make_linux_controller(Some(sinks));
        {
            let mut st = ctrl.state.lock().unwrap();
            st.connected_peer = "peer-1".to_string();
            st.connected_name = "Living Room".to_string();
            st.connected_host = "192.168.1.50".to_string();
            st.connected_port = 4713;
            st.connected_sink = "soundhop_peer_1".to_string();
            st.connected_module = 150;
        }

        let started = ctrl
            .start_web_broadcast(Some(0), Some(true), Some(true))
            .expect("web broadcast starts alongside peer session");
        assert!(started.active);
        assert_eq!(ctrl.state.lock().unwrap().connected_peer, "peer-1");
        assert_eq!(ctrl.state.lock().unwrap().connected_sink, "soundhop_peer_1");
        assert_eq!(ctrl.state.lock().unwrap().web_null_module, 0);

        let full_status = ctrl.status(None);
        assert_eq!(full_status["connection"]["connected"], true);
        assert_eq!(full_status["connection"]["peer_id"], "peer-1");
        assert_eq!(full_status["web_broadcast"]["active"], true);

        let stopped = ctrl.stop_web_broadcast().expect("stop succeeds");
        assert!(!stopped.active);
        assert_eq!(ctrl.state.lock().unwrap().connected_peer, "peer-1");
        assert_eq!(ctrl.state.lock().unwrap().connected_module, 150);
        assert!(!unloaded.lock().unwrap().contains(&150));
    }
}
