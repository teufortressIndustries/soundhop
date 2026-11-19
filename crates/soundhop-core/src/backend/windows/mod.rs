//! Windows `AudioBackend` implementation using the PulseAudio Native Protocol
//! over TCP and WASAPI playback/loopback, including Multi-Room fan-out support.

pub mod audio;
pub mod protocol;

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ipnet::IpNet;

use crate::backend::windows::audio::{
    list_audio_endpoints, WindowsLoopbackRecorder, WindowsPlayer,
};
use crate::backend::windows::protocol::{
    make_command_auth, make_command_create_playback_stream, make_command_set_client_name,
    make_reply_auth, make_reply_create_playback_stream, make_reply_ok, make_reply_server_info,
    read_packet, write_packet, SampleSpec, TagStructReader, PA_COMMAND_AUTH,
    PA_COMMAND_CREATE_PLAYBACK_STREAM, PA_COMMAND_GET_SERVER_INFO, PA_COMMAND_REPLY,
    PA_COMMAND_SET_CLIENT_NAME,
};
use crate::backend::{
    AudioBackend, BackendError, Listener, Module, Sink, SinkInput, SinkInputInfo, Source,
    SourceOutput, SourceTunnelParams, TunnelParams,
};
use crate::constants::SINK_PREFIX;
use crate::util::sanitize_sink_name;

struct ActiveTunnelSession {
    pub module_id: u32,
    stop_flag: Arc<AtomicBool>,
    recorder: Option<WindowsLoopbackRecorder>,
    writer_stream: Option<Arc<Mutex<TcpStream>>>,
    data_channel: u32,
}

impl ActiveTunnelSession {
    fn is_alive(&self) -> bool {
        !self.stop_flag.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(rec) = &self.recorder {
            rec.stop();
        }
    }
}

impl Drop for ActiveTunnelSession {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct WindowsAudioBackend {
    listener_stop: Mutex<Option<Arc<AtomicBool>>>,
    listener_thread: Mutex<Option<JoinHandle<()>>>,
    listener_port: Mutex<Option<u16>>,
    listener_security: Mutex<String>,
    listener_allowed_networks: Mutex<Vec<String>>,
    next_module_id: AtomicU32,
    modules: Mutex<Vec<Module>>,
    default_sink: Mutex<Option<String>>,
    active_senders: Mutex<HashMap<String, ActiveTunnelSession>>,
    volume_pct: AtomicU32,
    muted: AtomicBool,
}

impl Default for WindowsAudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WindowsAudioBackend {
    fn drop(&mut self) {
        self.stop_listener_internal();
        let mut senders = self
            .active_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (_, sess) in senders.drain() {
            sess.stop();
        }
    }
}

impl WindowsAudioBackend {
    pub fn new() -> Self {
        Self {
            listener_stop: Mutex::new(None),
            listener_thread: Mutex::new(None),
            listener_port: Mutex::new(None),
            listener_security: Mutex::new("trusted-lan".to_string()),
            listener_allowed_networks: Mutex::new(Vec::new()),
            next_module_id: AtomicU32::new(101),
            modules: Mutex::new(Vec::new()),
            default_sink: Mutex::new(None),
            active_senders: Mutex::new(HashMap::new()),
            volume_pct: AtomicU32::new(100),
            muted: AtomicBool::new(false),
        }
    }

    fn stop_listener_internal(&self) {
        if let Some(stop_flag) = self
            .listener_stop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            stop_flag.store(true, Ordering::SeqCst);
        }
        if let Some(port) = *self.listener_port.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], port)),
                Duration::from_millis(100),
            );
        }
        if let Some(handle) = self
            .listener_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
        *self.listener_port.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl AudioBackend for WindowsAudioBackend {
    fn available(&self) -> bool {
        true
    }

    fn invalidate(&self) {}

    fn server_name(&self) -> Result<String, BackendError> {
        Ok("Windows Audio (WASAPI)".to_string())
    }

    fn server_version(&self) -> Result<String, BackendError> {
        Ok("10.0".to_string())
    }

    fn is_pipewire(&self) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn default_sink(&self) -> Result<String, BackendError> {
        if let Some(def) = self.default_sink.lock().unwrap().clone() {
            if self.sink_exists(&def)? {
                return Ok(def);
            }
        }
        let eps = list_audio_endpoints();
        Ok(eps
            .first()
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "Default Output".to_string()))
    }

    fn default_source(&self) -> Result<String, BackendError> {
        Ok("Default Input".to_string())
    }

    fn modules(&self) -> Result<Vec<Module>, BackendError> {
        Ok(self.modules.lock().unwrap().clone())
    }

    fn sinks(&self) -> Result<Vec<Sink>, BackendError> {
        let mut out = Vec::new();
        for (idx, ep) in list_audio_endpoints().into_iter().enumerate() {
            out.push(Sink {
                id: (idx + 1) as u32,
                name: ep.name,
                driver: "WASAPI".to_string(),
                spec: format!("s16le {}ch {}Hz", ep.channels, ep.sample_rate),
                state: "RUNNING".to_string(),
            });
        }
        for (sink_name, session) in self.active_senders.lock().unwrap().iter() {
            out.push(Sink {
                id: session.module_id,
                name: sink_name.clone(),
                driver: "module-tunnel-sink".to_string(),
                spec: "s16le 2ch 48000Hz".to_string(),
                state: if session.is_alive() {
                    "RUNNING".to_string()
                } else {
                    "SUSPENDED".to_string()
                },
            });
        }
        Ok(out)
    }

    fn sources(&self) -> Result<Vec<Source>, BackendError> {
        Ok(vec![Source {
            id: 1,
            name: "Default Input".to_string(),
            driver: "WASAPI".to_string(),
            spec: "s16le 2ch 48000Hz".to_string(),
            state: "running".to_string(),
        }])
    }

    fn sink_inputs(&self) -> Result<Vec<SinkInput>, BackendError> {
        Ok(Vec::new())
    }

    fn detailed_sink_inputs(&self) -> Result<Vec<SinkInputInfo>, BackendError> {
        Ok(Vec::new())
    }

    fn source_outputs(&self) -> Result<Vec<SourceOutput>, BackendError> {
        Ok(Vec::new())
    }

    fn load_module(&self, name: &str, args: &[String]) -> Result<u32, BackendError> {
        let id = self.next_module_id.fetch_add(1, Ordering::SeqCst);
        self.modules.lock().unwrap().push(Module {
            id,
            name: name.to_string(),
            args: args.join(" "),
        });
        Ok(id)
    }

    fn unload_module(&self, module_id: u32, ignore_missing: bool) -> Result<bool, BackendError> {
        if module_id == 100 {
            self.stop_listener_internal();
            self.modules.lock().unwrap().retain(|m| m.id != 100);
            return Ok(true);
        }
        let mut senders = self.active_senders.lock().unwrap();
        let found_key = senders
            .iter()
            .find(|(_, s)| s.module_id == module_id)
            .map(|(k, _)| k.clone());
        if let Some(key) = found_key {
            if let Some(sess) = senders.remove(&key) {
                sess.stop();
            }
            self.modules.lock().unwrap().retain(|m| m.id != module_id);
            let mut def = self.default_sink.lock().unwrap();
            if def.as_deref() == Some(key.as_str()) {
                *def = None;
            }
            return Ok(true);
        }
        let mut mods = self.modules.lock().unwrap();
        let before = mods.len();
        mods.retain(|m| m.id != module_id);
        if mods.len() < before {
            return Ok(true);
        }
        if !ignore_missing {
            return Err(BackendError::Message(format!(
                "Could not unload module {module_id}: module not found"
            )));
        }
        Ok(false)
    }

    fn unload_tunnels(&self, sink_prefix: &str) -> Result<Vec<u32>, BackendError> {
        let mut removed = Vec::new();
        let mut senders = self.active_senders.lock().unwrap();
        let keys: Vec<String> = senders
            .keys()
            .filter(|k| k.starts_with(sink_prefix))
            .cloned()
            .collect();
        for k in keys {
            if let Some(sess) = senders.remove(&k) {
                sess.stop();
                removed.push(sess.module_id);
            }
        }
        self.modules
            .lock()
            .unwrap()
            .retain(|m| !removed.contains(&m.id));
        Ok(removed)
    }

    fn soundhop_modules(&self) -> Result<Vec<Module>, BackendError> {
        Ok(self
            .modules
            .lock()
            .unwrap()
            .iter()
            .filter(|m| {
                m.arg("sink_name")
                    .map(|s| s.starts_with(SINK_PREFIX))
                    .unwrap_or(false)
            })
            .cloned()
            .collect())
    }

    fn set_default_sink(&self, name: &str) -> Result<(), BackendError> {
        if name.trim().is_empty() {
            return Err(BackendError::Message(
                "Refusing to set an empty default sink".to_string(),
            ));
        }
        *self.default_sink.lock().unwrap() = Some(name.to_string());
        Ok(())
    }

    fn set_default_source(&self, _name: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn move_sink_inputs(
        &self,
        _target: &str,
        _source: Option<&str>,
    ) -> Result<usize, BackendError> {
        Ok(0)
    }

    fn move_source_outputs(
        &self,
        _target: &str,
        _source: Option<&str>,
        _exclude_modules: &[u32],
    ) -> Result<usize, BackendError> {
        Ok(0)
    }

    fn move_app_streams(&self, _identifier: &str, _target: &str) -> Result<usize, BackendError> {
        Err(BackendError::Message(
            "Per-application stream routing is only supported on Linux (PipeWire/PulseAudio)"
                .to_string(),
        ))
    }

    fn ensure_tcp_listener(
        &self,
        port: u16,
        security: &str,
        allowed_networks: &[String],
    ) -> Result<Listener, BackendError> {
        if let Some(active_port) = *self.listener_port.lock().unwrap_or_else(|e| e.into_inner()) {
            let current_sec = self
                .listener_security
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let current_nets = self
                .listener_allowed_networks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if active_port == port && current_sec == security && current_nets == allowed_networks {
                return Ok(Listener {
                    module_id: 100,
                    created: false,
                    args: format!("port={port} security={current_sec}"),
                    port,
                    security: current_sec,
                });
            }
        }
        self.stop_listener_internal();
        if matches!(security, "psk" | "pin" | "cookie") {
            let _ = crate::security::ensure_pa_cookie(None);
        }
        let tcp = TcpListener::bind(("0.0.0.0", port))
            .map_err(|e| BackendError::Message(format!("Could not bind TCP port {port}: {e}")))?;
        let bound_port = tcp.local_addr().map(|a| a.port()).unwrap_or(port);
        tcp.set_nonblocking(true).ok();

        let generation_stop = Arc::new(AtomicBool::new(false));
        *self.listener_stop.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Arc::clone(&generation_stop));
        *self.listener_port.lock().unwrap_or_else(|e| e.into_inner()) = Some(bound_port);
        *self
            .listener_security
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = security.to_string();
        *self
            .listener_allowed_networks
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = allowed_networks.to_vec();

        const MAX_CONCURRENT_PA_CLIENTS: usize = 16;
        let stop_clone = generation_stop;
        let sec_owned = security.to_string();
        let nets_owned: Vec<String> = effective_allowed_networks(allowed_networks);
        let active_clients = Arc::new(AtomicUsize::new(0));
        let handle = thread::spawn(move || {
            while !stop_clone.load(Ordering::SeqCst) {
                match tcp.accept() {
                    Ok((stream, peer_addr)) => {
                        if stop_clone.load(Ordering::SeqCst) {
                            break;
                        }
                        if sec_owned != "open" && !ip_allowed(&peer_addr.ip(), &nets_owned) {
                            continue;
                        }
                        let prev =
                            active_clients.try_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                                (cur < MAX_CONCURRENT_PA_CLIENTS).then_some(cur + 1)
                            });
                        if prev.is_err() {
                            drop(stream);
                            continue;
                        }
                        let client_stop = Arc::clone(&stop_clone);
                        let client_sec = sec_owned.clone();
                        let clients_ref = Arc::clone(&active_clients);
                        thread::spawn(move || {
                            let _slot_guard = ClientSlotGuard(clients_ref);
                            handle_incoming_pa_client(stream, client_stop, client_sec);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(40));
                    }
                    Err(_) => break,
                }
            }
        });
        *self.listener_thread.lock().unwrap() = Some(handle);

        let args_str = format!("port={bound_port} security={security}");
        let mut mods = self.modules.lock().unwrap();
        mods.retain(|m| m.id != 100);
        mods.push(Module {
            id: 100,
            name: "module-native-protocol-tcp".to_string(),
            args: args_str.clone(),
        });

        Ok(Listener {
            module_id: 100,
            created: true,
            args: args_str,
            port: bound_port,
            security: security.to_string(),
        })
    }

    fn connect_tunnel(&self, params: &TunnelParams<'_>) -> Result<u32, BackendError> {
        let sink_name = sanitize_sink_name(params.sink_name, 64);
        let addr = (params.host, params.port)
            .to_socket_addrs()
            .map_err(|e| BackendError::Message(format!("Cannot resolve {}: {e}", params.host)))?
            .next()
            .ok_or_else(|| BackendError::Message("No address found".to_string()))?;

        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
            .map_err(|e| BackendError::Message(format!("Cannot connect to {addr}: {e}")))?;
        stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(3))).ok();

        // PulseAudio Native Protocol Handshake
        let cookie = load_cookie_bytes().unwrap_or([0u8; 256]);
        stream
            .write_all(&make_command_auth(0, 35, &cookie))
            .map_err(|e| BackendError::Message(format!("PA auth send failed: {e}")))?;
        let auth_reply = read_packet(&mut stream)
            .map_err(|e| BackendError::Message(format!("No PA auth reply: {e}")))?;
        let mut r = TagStructReader::new(&auth_reply.data);
        if r.read_u32().unwrap_or(0) != PA_COMMAND_REPLY {
            return Err(BackendError::Message("PA auth rejected".to_string()));
        }

        stream
            .write_all(&make_command_set_client_name(1, "SoundHop-Windows"))
            .ok();
        let _ = read_packet(&mut stream);

        stream
            .write_all(&make_command_create_playback_stream(
                2,
                None,
                SampleSpec::default(),
            ))
            .map_err(|e| BackendError::Message(format!("PA create stream failed: {e}")))?;
        let stream_reply = read_packet(&mut stream)
            .map_err(|e| BackendError::Message(format!("No PA stream reply: {e}")))?;
        let mut sr = TagStructReader::new(&stream_reply.data);
        if sr.read_u32().unwrap_or(0) != PA_COMMAND_REPLY {
            return Err(BackendError::Message(
                "PA playback stream rejected".to_string(),
            ));
        }
        let _tag = sr.read_u32().unwrap_or(0);
        let _stream_index = sr.read_u32().unwrap_or(0);
        let data_channel = sr.read_u32().unwrap_or(0);

        let mod_id = self.next_module_id.fetch_add(1, Ordering::SeqCst);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let writer_stream = Arc::new(Mutex::new(stream));
        let writer_for_rec = Arc::clone(&writer_stream);
        let recorder = WindowsLoopbackRecorder::try_new(move |pcm_chunk| {
            let pkt = write_packet(data_channel, pcm_chunk);
            if let Ok(mut guard) = writer_for_rec.lock() {
                let _ = guard.write_all(&pkt);
            }
        })
        .map_err(BackendError::Message)?;

        let replaced = self
            .active_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                sink_name.clone(),
                ActiveTunnelSession {
                    module_id: mod_id,
                    stop_flag,
                    recorder: Some(recorder),
                    writer_stream: Some(writer_stream),
                    data_channel,
                },
            );
        if let Some(old_sess) = replaced {
            let old_id = old_sess.module_id;
            old_sess.stop();
            self.modules
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|m| m.id != old_id);
        }
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Module {
                id: mod_id,
                name: "module-tunnel-sink".to_string(),
                args: format!(
                    "server=tcp:{}:{} sink_name={sink_name}",
                    params.host, params.port
                ),
            });
        Ok(mod_id)
    }

    fn disconnect_tunnel(&self, sink_name: &str) -> Result<bool, BackendError> {
        let mod_id = self
            .active_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sink_name)
            .map(|s| s.module_id);
        if let Some(id) = mod_id {
            return self.unload_module(id, true);
        }
        Ok(false)
    }

    fn create_combined_sink(
        &self,
        sink_name: &str,
        _description: &str,
        slave_sinks: &[String],
    ) -> Result<u32, BackendError> {
        let sink_name = sanitize_sink_name(sink_name, 64);
        if let Some(old_sess) = self
            .active_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&sink_name)
        {
            let old_id = old_sess.module_id;
            old_sess.stop();
            self.modules
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|m| m.id != old_id);
        }

        let mod_id = self.next_module_id.fetch_add(1, Ordering::SeqCst);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut slave_recorders_to_stop = Vec::new();
        let targets: Vec<(Arc<Mutex<TcpStream>>, u32)> = {
            let mut senders = self
                .active_senders
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            slave_sinks
                .iter()
                .filter_map(|name| {
                    senders.get_mut(name).and_then(|sess| {
                        let target = sess
                            .writer_stream
                            .as_ref()
                            .map(|ws| (Arc::clone(ws), sess.data_channel));
                        if target.is_some() {
                            if let Some(rec) = sess.recorder.take() {
                                slave_recorders_to_stop.push(rec);
                            }
                        }
                        target
                    })
                })
                .collect()
        };
        for rec in slave_recorders_to_stop {
            rec.stop();
        }
        let recorder = if targets.is_empty() {
            None
        } else {
            Some(
                WindowsLoopbackRecorder::try_new(move |pcm_chunk| {
                    for (ws, ch) in &targets {
                        let pkt = write_packet(*ch, pcm_chunk);
                        if let Ok(mut guard) = ws.lock() {
                            let _ = guard.write_all(&pkt);
                        }
                    }
                })
                .map_err(BackendError::Message)?,
            )
        };
        let replaced = self
            .active_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                sink_name.clone(),
                ActiveTunnelSession {
                    module_id: mod_id,
                    stop_flag,
                    recorder,
                    writer_stream: None,
                    data_channel: 0,
                },
            );
        if let Some(old_sess) = replaced {
            let old_id = old_sess.module_id;
            old_sess.stop();
            self.modules
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|m| m.id != old_id);
        }
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Module {
                id: mod_id,
                name: "module-combine-sink".to_string(),
                args: format!("sink_name={sink_name} slaves={}", slave_sinks.join(",")),
            });
        Ok(mod_id)
    }

    fn connect_tunnel_source(&self, _params: &SourceTunnelParams<'_>) -> Result<u32, BackendError> {
        Err(BackendError::Message(
            "Microphone tunnel sources require PipeWire/PulseAudio".to_string(),
        ))
    }

    fn disconnect_tunnel_source(&self, _source_name: &str) -> Result<bool, BackendError> {
        Ok(false)
    }

    fn send_mic_to_remote(
        &self,
        _host: &str,
        _port: u16,
        _description: &str,
        _local_source: &str,
    ) -> Result<(Option<u32>, Option<u32>, u32, u32, String), BackendError> {
        Err(BackendError::Message(
            "Microphone pass-through requires PipeWire/PulseAudio".to_string(),
        ))
    }

    fn stop_send_mic_to_remote(
        &self,
        _host: &str,
        _port: u16,
        _remote_null_id: Option<u32>,
        _remote_remap_id: Option<u32>,
        _local_tunnel_id: u32,
        _local_loopback_id: u32,
    ) {
    }

    fn cleanup_orphaned_remote_mic_modules(&self) -> Result<Vec<u32>, BackendError> {
        Ok(Vec::new())
    }

    fn get_sink_volume(&self, _sink_name: &str) -> u32 {
        self.volume_pct.load(Ordering::SeqCst)
    }

    fn set_sink_volume(&self, _sink_name: &str, volume_percent: u32) {
        self.volume_pct
            .store(volume_percent.min(150), Ordering::SeqCst);
    }

    fn get_sink_mute(&self, _sink_name: &str) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    fn set_sink_mute(&self, _sink_name: &str, mute: bool) {
        self.muted.store(mute, Ordering::SeqCst);
    }

    fn remote_set_volume(&self, _host: &str, _port: u16, volume_percent: u32, mute: Option<bool>) {
        self.set_sink_volume("", volume_percent);
        if let Some(m) = mute {
            self.set_sink_mute("", m);
        }
    }
}

fn effective_allowed_networks(allowed_networks: &[String]) -> Vec<String> {
    if !allowed_networks.is_empty() {
        return allowed_networks.to_vec();
    }
    let mut nets: Vec<String> = crate::constants::DEFAULT_LAN_NETWORKS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    nets.push("169.254.0.0/16".to_string());
    nets.push("fe80::/10".to_string());
    nets
}

fn ip_allowed(ip: &IpAddr, allowed_networks: &[String]) -> bool {
    if ip.is_loopback() {
        return true;
    }
    for net_str in allowed_networks {
        if let Ok(net) = net_str.parse::<IpNet>() {
            if net.contains(ip) {
                return true;
            }
        } else if let Ok(single) = net_str.parse::<IpAddr>() {
            if &single == ip {
                return true;
            }
        }
    }
    false
}

struct ClientSlotGuard(Arc<AtomicUsize>);

impl Drop for ClientSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn load_cookie_bytes() -> Option<[u8; 256]> {
    let cookie_path = crate::security::soundhop_cookie_path();
    let bytes = std::fs::read(&cookie_path).ok()?;
    if bytes.len() < 256 {
        return None;
    }
    let mut cookie = [0u8; 256];
    cookie.copy_from_slice(&bytes[..256]);
    Some(cookie)
}

fn handle_incoming_pa_client(stream: TcpStream, stop_flag: Arc<AtomicBool>, security: String) {
    let cookie_required = matches!(security.as_str(), "psk" | "pin" | "cookie");
    let expected_cookie = if cookie_required {
        match load_cookie_bytes() {
            Some(c) => Some(c),
            None => return,
        }
    } else {
        None
    };
    handle_incoming_pa_client_with_cookie(stream, stop_flag, cookie_required, expected_cookie);
}

fn handle_incoming_pa_client_with_cookie(
    mut stream: TcpStream,
    stop_flag: Arc<AtomicBool>,
    cookie_required: bool,
    expected_cookie: Option<[u8; 256]>,
) {
    if cookie_required && expected_cookie.is_none() {
        return;
    }
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

    let mut authenticated = !cookie_required;
    let mut player: Option<WindowsPlayer> = None;

    while !stop_flag.load(Ordering::SeqCst) {
        let pkt = match read_packet(&mut stream) {
            Ok(pkt) => pkt,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        if pkt.is_control() {
            let mut r = TagStructReader::new(&pkt.data);
            let Ok(cmd) = r.read_u32() else {
                continue;
            };
            let Ok(tag) = r.read_u32() else {
                continue;
            };
            let reply = match cmd {
                PA_COMMAND_AUTH => {
                    let _proto_ver = r.read_u32();
                    let client_cookie = r.read_arbitrary().unwrap_or_default();
                    if let Some(ref exp) = expected_cookie {
                        if !crate::security::constant_time_eq(&client_cookie, exp) {
                            break;
                        }
                    }
                    authenticated = true;
                    Some(make_reply_auth(tag, 35))
                }
                _ if !authenticated => {
                    break;
                }
                PA_COMMAND_SET_CLIENT_NAME => Some(make_reply_ok(tag)),
                PA_COMMAND_GET_SERVER_INFO => Some(make_reply_server_info(tag)),
                PA_COMMAND_CREATE_PLAYBACK_STREAM => {
                    let spec = r.read_sample_spec().unwrap_or_default();
                    if player.is_none() {
                        let wp = WindowsPlayer::new(spec.rate.max(8_000), spec.channels.max(1));
                        wp.start();
                        player = Some(wp);
                    }
                    Some(make_reply_create_playback_stream(tag, 0, 0, spec))
                }
                _ => Some(make_reply_ok(tag)),
            };
            if let Some(bytes) = reply {
                if stream.write_all(&bytes).is_err() {
                    break;
                }
            }
        } else if authenticated {
            let p = player.get_or_insert_with(|| {
                let wp = WindowsPlayer::new(48_000, 2);
                wp.start();
                wp
            });
            p.write(&pkt.data);
        } else {
            break;
        }
    }
    if let Some(p) = player {
        p.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_windows_backend_listener_and_tunnel_loopback() {
        let receiver = WindowsAudioBackend::new();
        let listener = receiver
            .ensure_tcp_listener(0, "open", &["127.0.0.1".to_string()])
            .unwrap();
        assert!(listener.port > 0);

        let sender = WindowsAudioBackend::new();
        let mod_id = sender
            .connect_tunnel(&TunnelParams {
                host: "127.0.0.1",
                port: listener.port,
                sink_name: "soundhop_test_win",
                description: "WinPeer",
                reconnect_delay_ms: 1000,
                latency_ms: None,
                extra_args: &[],
            })
            .unwrap();
        assert!(mod_id >= 101);
        assert!(sender.sink_exists("soundhop_test_win").unwrap());

        sender.disconnect_tunnel("soundhop_test_win").unwrap();
        receiver.remove_listener(listener.module_id).unwrap();
    }

    #[test]
    fn test_windows_pa_client_cookie_fail_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);

        // Missing cookie fails closed immediately
        let h1 = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handle_incoming_pa_client_with_cookie(s, stop_c, true, None);
        });
        let mut c1 = TcpStream::connect(addr).unwrap();
        let _ = c1.write_all(&make_command_auth(0, 35, &[0u8; 256]));
        assert!(read_packet(&mut c1).is_err());
        h1.join().unwrap();

        // Wrong cookie is rejected; right cookie succeeds
        let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let expected = [0x42u8; 256];
        let stop_c2 = Arc::clone(&stop);
        let h2 = thread::spawn(move || {
            for _ in 0..2 {
                let (s, _) = listener2.accept().unwrap();
                let sc = Arc::clone(&stop_c2);
                handle_incoming_pa_client_with_cookie(s, sc, true, Some(expected));
            }
        });

        let mut bad_client = TcpStream::connect(addr2).unwrap();
        bad_client
            .write_all(&make_command_auth(0, 35, &[0x99u8; 256]))
            .unwrap();
        assert!(read_packet(&mut bad_client).is_err());

        let mut good_client = TcpStream::connect(addr2).unwrap();
        good_client
            .write_all(&make_command_auth(0, 35, &expected))
            .unwrap();
        let reply = read_packet(&mut good_client).unwrap();
        let mut r = TagStructReader::new(&reply.data);
        assert_eq!(r.read_u32().unwrap(), PA_COMMAND_REPLY);
        drop(good_client);
        stop.store(true, Ordering::SeqCst);
        h2.join().unwrap();
    }

    #[test]
    fn test_windows_listener_restart_uses_new_generation_stop_flag() {
        let receiver = WindowsAudioBackend::new();
        let l1 = receiver
            .ensure_tcp_listener(0, "open", &["127.0.0.1".to_string()])
            .unwrap();

        let mut old_client = TcpStream::connect(("127.0.0.1", l1.port)).unwrap();
        old_client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        old_client
            .write_all(&make_command_auth(0, 35, &[0u8; 256]))
            .unwrap();
        let reply = read_packet(&mut old_client).unwrap();
        assert_eq!(
            TagStructReader::new(&reply.data).read_u32().unwrap(),
            PA_COMMAND_REPLY
        );

        // Restart listener (new generation) while old_client is still connected.
        let l2 = receiver
            .ensure_tcp_listener(0, "trusted-lan", &["127.0.0.1".to_string()])
            .unwrap();
        assert!(l2.created);

        // Old client's handler must see its generation stop flag = true and close the connection,
        // even though the new listener generation is active.
        let res = read_packet(&mut old_client);
        assert!(
            res.is_err(),
            "old client should be disconnected after listener restart"
        );

        receiver.remove_listener(l2.module_id).unwrap();
    }

    #[test]
    fn test_windows_empty_allowed_networks_defaults_to_private_lan_acl() {
        let nets = effective_allowed_networks(&[]);
        assert!(ip_allowed(&"127.0.0.1".parse().unwrap(), &nets));
        assert!(ip_allowed(&"192.168.1.42".parse().unwrap(), &nets));
        assert!(ip_allowed(&"10.0.0.5".parse().unwrap(), &nets));
        assert!(ip_allowed(&"172.16.5.1".parse().unwrap(), &nets));
        assert!(ip_allowed(&"169.254.10.20".parse().unwrap(), &nets));
        assert!(ip_allowed(&"fe80::1".parse().unwrap(), &nets));
        // Public IPs must be rejected (fail-closed)
        assert!(!ip_allowed(&"8.8.8.8".parse().unwrap(), &nets));
        assert!(!ip_allowed(&"203.0.113.5".parse().unwrap(), &nets));
        assert!(!ip_allowed(&"2001:db8::1".parse().unwrap(), &nets));
    }

    #[test]
    fn test_windows_multiroom_stops_per_tunnel_recorders_and_replaces_duplicates() {
        let r1 = WindowsAudioBackend::new();
        let l1 = r1
            .ensure_tcp_listener(0, "open", &["127.0.0.1".to_string()])
            .unwrap();
        let r2 = WindowsAudioBackend::new();
        let l2 = r2
            .ensure_tcp_listener(0, "open", &["127.0.0.1".to_string()])
            .unwrap();

        let sender = WindowsAudioBackend::new();
        let m1 = sender
            .connect_tunnel(&TunnelParams {
                host: "127.0.0.1",
                port: l1.port,
                sink_name: "soundhop_room_a",
                description: "RoomA",
                reconnect_delay_ms: 1000,
                latency_ms: None,
                extra_args: &[],
            })
            .unwrap();
        // Reconnecting with the same sink_name must replace & clean up the old module
        let m1_new = sender
            .connect_tunnel(&TunnelParams {
                host: "127.0.0.1",
                port: l1.port,
                sink_name: "soundhop_room_a",
                description: "RoomA",
                reconnect_delay_ms: 1000,
                latency_ms: None,
                extra_args: &[],
            })
            .unwrap();
        assert_ne!(m1, m1_new);
        assert!(sender.module(m1).unwrap().is_none());

        let _m2 = sender
            .connect_tunnel(&TunnelParams {
                host: "127.0.0.1",
                port: l2.port,
                sink_name: "soundhop_room_b",
                description: "RoomB",
                reconnect_delay_ms: 1000,
                latency_ms: None,
                extra_args: &[],
            })
            .unwrap();

        let _comb = sender
            .create_combined_sink(
                "soundhop_multiroom",
                "AllRooms",
                &["soundhop_room_a".to_string(), "soundhop_room_b".to_string()],
            )
            .unwrap();

        // Slave per-tunnel recorders must be stopped/cleared when combined sink takes over
        {
            let senders = sender.active_senders.lock().unwrap();
            assert!(senders.get("soundhop_room_a").unwrap().recorder.is_none());
            assert!(senders.get("soundhop_room_b").unwrap().recorder.is_none());
            assert!(senders
                .get("soundhop_multiroom")
                .unwrap()
                .recorder
                .is_some());
        }

        sender.unload_tunnels("soundhop_").unwrap();
        r1.remove_listener(l1.module_id).unwrap();
        r2.remove_listener(l2.module_id).unwrap();
    }

    #[test]
    fn test_windows_idle_client_stays_connected_across_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);

        let h = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            handle_incoming_pa_client_with_cookie(s, stop_c, false, None);
        });

        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .write_all(&make_command_auth(0, 35, &[0u8; 256]))
            .unwrap();
        let _ = read_packet(&mut client).unwrap();

        // Remain idle longer than the server's 500ms read timeout
        thread::sleep(Duration::from_millis(650));

        // Server must still be connected and answer subsequent commands
        client
            .write_all(&make_command_set_client_name(1, "IdleTest"))
            .unwrap();
        let reply = read_packet(&mut client).unwrap();
        assert_eq!(
            TagStructReader::new(&reply.data).read_u32().unwrap(),
            PA_COMMAND_REPLY
        );

        stop.store(true, Ordering::SeqCst);
        h.join().unwrap();
    }
}
