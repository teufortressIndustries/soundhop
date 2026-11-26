//! WebSocket PCM16LE fallback fan-out (`web-ws-fanout`) backed by `tungstenite`.

use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::transport::{BroadcastFrame, BroadcastSubscription, SubscriberStats};
use crate::util::{lock_or_recover, now_secs};

use super::{BroadcastRuntime, ClientSlotGuard, ListenerGuard, WebClientInfo, WebError, WebResult};

/// 2-byte magic prefix (`"SH"`) on every binary WebSocket audio frame.
pub const WS_FRAME_MAGIC: [u8; 2] = [0x53, 0x48];
/// Wire version byte on binary WebSocket audio frames.
pub const WS_FRAME_VERSION: u8 = 0x01;
/// Frame flag bit 0: interleaved 48 kHz stereo PCM16LE payload.
pub const WS_FLAG_PCM16LE_48K_STEREO: u8 = 0x01;
/// Frame flag bit 1: stream discontinuity / reset indicator.
pub const WS_FLAG_DISCONTINUITY: u8 = 0x02;
/// Fixed little-endian binary WebSocket frame header size in bytes.
pub const WS_HEADER_LEN: usize = 16;
/// Negotiated WebSocket subprotocol echoed in `Sec-WebSocket-Protocol`.
pub const WS_SUBPROTOCOL: &str = "soundhop.v1";
/// Terminal close code sent when the host kicks a listener. The browser must
/// treat this as a permanent stop and must not auto-reconnect.
pub const WS_KICK_CLOSE_CODE: u16 = 4003;
pub const WS_KICK_REASON: &str = "kicked by host";

const WS_AUTH_TIMEOUT: Duration = Duration::from_secs(2);
const WS_AUTH_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WS_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const WS_CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_millis(150);
const WS_RX_POLL_TIMEOUT: Duration = Duration::from_millis(5);
const WS_MAX_INCOMING_MSG_BYTES: usize = 16 * 1024;
const WS_MAX_WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// Encode a binary WebSocket PCM16LE audio frame with the 16-byte SoundHop header.
pub fn encode_ws_audio_frame(seq: u32, sample_pos: u64, flags: u8, pcm_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(WS_HEADER_LEN + pcm_payload.len());
    out.extend_from_slice(&WS_FRAME_MAGIC);
    out.push(WS_FRAME_VERSION);
    out.push(flags);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&sample_pos.to_le_bytes());
    out.extend_from_slice(pcm_payload);
    out
}

/// Configuration for starting `web-ws-fanout`.
#[derive(Clone)]
pub struct WsFanoutConfig {
    pub runtime: Arc<BroadcastRuntime>,
}

/// Accepted TCP connection handed off from `web-http-accept` on `GET /ws`.
pub struct WsUpgradeRequest {
    pub stream: TcpStream,
    pub remote_addr: SocketAddr,
    pub user_agent: String,
    pub sec_websocket_key: String,
    pub subprotocol_token: Option<String>,
    pub client_sent_soundhop_protocol: bool,
    pub leftover_bytes: Vec<u8>,
}

/// Stream wrapper that replays already-buffered HTTP bytes before reading from `TcpStream`.
struct PrefixedStream {
    head: Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl PrefixedStream {
    fn new(leftover_bytes: Vec<u8>, inner: TcpStream) -> Self {
        Self {
            head: Cursor::new(leftover_bytes),
            inner,
        }
    }

    fn inner_mut(&mut self) -> &mut TcpStream {
        &mut self.inner
    }
}

impl Read for PrefixedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.head.position() as usize;
        if pos < self.head.get_ref().len() {
            let n = self.head.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
        }
        self.inner.read(buf)
    }
}

impl Write for PrefixedStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Deserialize)]
struct WsAuthFrame {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    token: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WsClientMessage {
    #[serde(rename = "ping")]
    Ping {
        #[serde(default)]
        t: Value,
    },
    #[serde(rename = "stats")]
    Stats {
        #[serde(rename = "rttMs", default)]
        rtt_ms: Option<f64>,
    },
}

struct WsClientState {
    id: String,
    remote_addr: String,
    user_agent: String,
    connected_since: f64,
    bytes_sent: AtomicU64,
    rtt_ms: Mutex<Option<f64>>,
    local_dropped_frames: AtomicU32,
    sub_stats: Arc<SubscriberStats>,
    kicked: AtomicBool,
    /// Non-zero when this client was explicitly kicked by the host (vs. a
    /// normal shutdown), so the wire loop can send a terminal close frame.
    kick_code: AtomicU16,
    stream: TcpStream,
}

impl WsClientState {
    fn to_info(&self) -> WebClientInfo {
        let dropped = self
            .sub_stats
            .dropped_frames
            .load(Ordering::Relaxed)
            .saturating_add(self.local_dropped_frames.load(Ordering::Relaxed));
        WebClientInfo {
            id: self.id.clone(),
            remote_addr: self.remote_addr.clone(),
            transport: "ws".to_string(),
            user_agent: self.user_agent.clone(),
            connected_since: self.connected_since,
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            rtt_ms: *lock_or_recover(&self.rtt_ms),
            dropped_frames: dropped,
        }
    }
}

/// Handle to the running `web-ws-fanout` subsystem.
pub struct WsFanoutHandle {
    runtime: Arc<BroadcastRuntime>,
    stop_flag: Arc<AtomicBool>,
    next_client_id: AtomicU64,
    clients: Arc<Mutex<HashMap<String, Arc<WsClientState>>>>,
    threads: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl WsFanoutHandle {
    /// Start the `web-ws-fanout` subsystem.
    pub fn start(config: WsFanoutConfig) -> WebResult<Self> {
        Ok(Self {
            runtime: config.runtime,
            stop_flag: Arc::new(AtomicBool::new(false)),
            next_client_id: AtomicU64::new(1),
            clients: Arc::new(Mutex::new(HashMap::new())),
            threads: Mutex::new(HashMap::new()),
        })
    }

    /// Complete the WebSocket upgrade/auth flow and register the client for PCM fan-out.
    pub fn accept_upgraded(&self, mut req: WsUpgradeRequest) -> WebResult<String> {
        if self.stop_flag.load(Ordering::Acquire) {
            return Err(WebError::NotRunning);
        }

        let ws_key = req.sec_websocket_key.trim();
        if ws_key.is_empty() {
            write_http_error(
                &mut req.stream,
                "400 Bad Request",
                "missing Sec-WebSocket-Key",
            );
            return Err(WebError::BadRequest("missing Sec-WebSocket-Key".into()));
        }

        let slot_guard = match self.runtime.try_acquire_client_slot() {
            Ok(guard) => guard,
            Err(err) => {
                write_http_error(&mut req.stream, "503 Service Unavailable", &err.to_string());
                return Err(err);
            }
        };

        let _ = req.stream.set_nodelay(true);
        let _ = req.stream.set_write_timeout(Some(WS_WRITE_TIMEOUT));
        let _ = req.stream.set_read_timeout(Some(WS_AUTH_POLL_INTERVAL));

        write_switching_protocols(&mut req.stream, ws_key, req.client_sent_soundhop_protocol)?;

        let kick_stream = req.stream.try_clone()?;
        let prefixed = PrefixedStream::new(req.leftover_bytes, req.stream);
        let mut ws = WebSocket::from_raw_socket(prefixed, Role::Server, Some(ws_config()));

        if self.runtime.token_required() {
            if let Some(ref tok) = req.subprotocol_token {
                if !self.runtime.verify_token(tok) {
                    send_close_and_drain(&mut ws, CloseCode::Policy, "invalid token");
                    return Err(WebError::Unauthorized(
                        "invalid WebSocket subprotocol token".into(),
                    ));
                }
            } else if let Err(err) =
                authenticate_first_frame(&mut ws, &self.runtime, &self.stop_flag)
            {
                let (code, reason) = match &err {
                    WebError::NotRunning => (CloseCode::Away, "server stopping"),
                    _ => (CloseCode::Policy, "authentication failed"),
                };
                send_close_and_drain(&mut ws, code, reason);
                return Err(err);
            }
        }

        let listener_guard = match self.runtime.acquire_listener() {
            Ok(guard) => guard,
            Err(err) => {
                send_close_and_drain(&mut ws, CloseCode::Error, "audio capture unavailable");
                return Err(err);
            }
        };

        let sub = self.runtime.registry().subscribe_pcm(None);
        let client_id = format!("ws-{}", self.next_client_id.fetch_add(1, Ordering::Relaxed));

        let client_state = Arc::new(WsClientState {
            id: client_id.clone(),
            remote_addr: req.remote_addr.to_string(),
            user_agent: req.user_agent,
            connected_since: now_secs(),
            bytes_sent: AtomicU64::new(0),
            rtt_ms: Mutex::new(None),
            local_dropped_frames: AtomicU32::new(0),
            sub_stats: Arc::clone(sub.stats()),
            kicked: AtomicBool::new(false),
            kick_code: AtomicU16::new(0),
            stream: kick_stream,
        });

        let mut threads = lock_or_recover(&self.threads);
        if self.stop_flag.load(Ordering::Acquire) {
            return Err(WebError::NotRunning);
        }
        threads.retain(|_, handle| !handle.is_finished());

        lock_or_recover(&self.clients).insert(client_id.clone(), Arc::clone(&client_state));

        let clients_ref = Arc::clone(&self.clients);
        let stop_flag = Arc::clone(&self.stop_flag);
        let state_ref = Arc::clone(&client_state);
        let thread_name = format!("web-ws-{client_id}");

        let handle = match thread::Builder::new().name(thread_name).spawn(move || {
            run_ws_client_loop(
                ws,
                sub,
                state_ref,
                clients_ref,
                stop_flag,
                slot_guard,
                listener_guard,
            );
        }) {
            Ok(handle) => handle,
            Err(err) => {
                lock_or_recover(&self.clients).remove(&client_id);
                return Err(WebError::Io(err));
            }
        };

        threads.insert(client_id.clone(), handle);
        Ok(client_id)
    }

    /// Forcefully close and remove a WebSocket client by ID. Returns `true` if found.
    ///
    /// The wire loop sends a terminal close frame (`WS_KICK_CLOSE_CODE`) before
    /// tearing down the socket so the browser stops instead of reconnecting.
    pub fn kick_client(&self, client_id: &str) -> bool {
        let removed = lock_or_recover(&self.clients).remove(client_id);
        if let Some(client) = removed {
            client
                .kick_code
                .store(WS_KICK_CLOSE_CODE, Ordering::Release);
            client.kicked.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Snapshot currently active WebSocket clients.
    pub fn list_clients(&self) -> Vec<WebClientInfo> {
        let mut out: Vec<WebClientInfo> = lock_or_recover(&self.clients)
            .values()
            .map(|c| c.to_info())
            .collect();
        out.sort_by(|a, b| {
            a.connected_since
                .partial_cmp(&b.connected_since)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// Current number of active WebSocket clients.
    pub fn client_count(&self) -> usize {
        lock_or_recover(&self.clients).len()
    }

    /// Stop `web-ws-fanout` and disconnect all WebSocket clients.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);

        let active: Vec<Arc<WsClientState>> = {
            let mut clients = lock_or_recover(&self.clients);
            clients.drain().map(|(_, c)| c).collect()
        };
        for client in active {
            client.kicked.store(true, Ordering::Release);
            let _ = client.stream.shutdown(Shutdown::Both);
        }

        let handles: Vec<JoinHandle<()>> = {
            let mut threads = lock_or_recover(&self.threads);
            threads.drain().map(|(_, h)| h).collect()
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl Drop for WsFanoutHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(4 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(WS_MAX_WRITE_BUFFER_BYTES)
        .max_message_size(Some(WS_MAX_INCOMING_MSG_BYTES))
        .max_frame_size(Some(WS_MAX_INCOMING_MSG_BYTES))
}

fn write_http_error(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn write_switching_protocols(
    stream: &mut TcpStream,
    sec_websocket_key: &str,
    echo_subprotocol: bool,
) -> WebResult<()> {
    let accept_key = derive_accept_key(sec_websocket_key.as_bytes());
    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept_key}\r\n"
    );
    if echo_subprotocol {
        resp.push_str(&format!("Sec-WebSocket-Protocol: {WS_SUBPROTOCOL}\r\n"));
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn authenticate_first_frame(
    ws: &mut WebSocket<PrefixedStream>,
    runtime: &BroadcastRuntime,
    stop_flag: &AtomicBool,
) -> WebResult<()> {
    let deadline = Instant::now() + WS_AUTH_TIMEOUT;
    loop {
        if stop_flag.load(Ordering::Acquire) {
            return Err(WebError::NotRunning);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(WebError::Unauthorized("WebSocket auth timeout".into()));
        }
        let remaining = deadline.saturating_duration_since(now);
        let step = remaining
            .min(WS_AUTH_POLL_INTERVAL)
            .max(Duration::from_millis(1));
        let _ = ws.get_mut().inner_mut().set_read_timeout(Some(step));

        match ws.read() {
            Ok(Message::Text(txt)) => {
                let Ok(auth) = serde_json::from_str::<WsAuthFrame>(&txt) else {
                    return Err(WebError::Unauthorized("malformed JSON auth frame".into()));
                };
                if auth.msg_type != "auth" {
                    return Err(WebError::Unauthorized(
                        "expected auth frame before audio".into(),
                    ));
                }
                if runtime.verify_token(&auth.token) {
                    return Ok(());
                }
                return Err(WebError::Unauthorized("invalid WebSocket token".into()));
            }
            Ok(Message::Ping(_)) => {
                let _ = ws.flush();
            }
            Ok(Message::Close(_)) => {
                return Err(WebError::Unauthorized(
                    "client closed before authenticating".into(),
                ));
            }
            Ok(_) => {
                return Err(WebError::Unauthorized(
                    "expected text JSON auth frame".into(),
                ));
            }
            Err(WsError::Io(ref e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => {
                return Err(WebError::Unauthorized(
                    "connection closed during auth".into(),
                ));
            }
            Err(err) => {
                return Err(WebError::WebSocket(err.to_string()));
            }
        }
    }
}

fn send_close_and_drain(ws: &mut WebSocket<PrefixedStream>, code: CloseCode, reason: &str) {
    let _ = ws.get_mut().inner_mut().set_nonblocking(false);
    let _ = ws
        .get_mut()
        .inner_mut()
        .set_write_timeout(Some(WS_WRITE_TIMEOUT));
    let _ = ws
        .get_mut()
        .inner_mut()
        .set_read_timeout(Some(WS_CLOSE_DRAIN_TIMEOUT));

    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    if ws.close(Some(frame)).is_ok() {
        let _ = ws.flush();
        let deadline = Instant::now() + WS_CLOSE_DRAIN_TIMEOUT;
        while Instant::now() < deadline {
            match ws.read() {
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
    let _ = ws.get_mut().inner_mut().shutdown(Shutdown::Both);
}

fn run_ws_client_loop(
    mut ws: WebSocket<PrefixedStream>,
    sub: BroadcastSubscription,
    state: Arc<WsClientState>,
    clients: Arc<Mutex<HashMap<String, Arc<WsClientState>>>>,
    stop_flag: Arc<AtomicBool>,
    _slot_guard: ClientSlotGuard,
    _listener_guard: ListenerGuard,
) {
    let _ = ws.get_mut().inner_mut().set_nodelay(true);
    let _ = ws
        .get_mut()
        .inner_mut()
        .set_write_timeout(Some(WS_WRITE_TIMEOUT));

    let mut last_seq: Option<u64> = None;
    let mut last_pts_dur: Option<(u64, u32)> = None;
    let mut send_close_on_exit = false;

    'client_loop: loop {
        if stop_flag.load(Ordering::Acquire)
            || state.kicked.load(Ordering::Acquire)
            || sub.is_evicted()
        {
            send_close_on_exit = sub.is_evicted() && !state.kicked.load(Ordering::Acquire);
            break;
        }

        if !drain_incoming_ws(&mut ws, &state) {
            break;
        }

        match sub.rx().recv_timeout(WS_RX_POLL_TIMEOUT) {
            Ok(frame) => {
                if !send_broadcast_frame(&mut ws, &state, &frame, &mut last_seq, &mut last_pts_dur)
                {
                    break;
                }
                while let Ok(next_frame) = sub.rx().try_recv() {
                    if stop_flag.load(Ordering::Acquire) || state.kicked.load(Ordering::Acquire) {
                        break 'client_loop;
                    }
                    if !send_broadcast_frame(
                        &mut ws,
                        &state,
                        &next_frame,
                        &mut last_seq,
                        &mut last_pts_dur,
                    ) {
                        break 'client_loop;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                send_close_on_exit = true;
                break;
            }
        }
    }

    lock_or_recover(&clients).remove(&state.id);
    let kick_code = state.kick_code.load(Ordering::Acquire);
    if kick_code != 0 {
        send_close_and_drain(&mut ws, CloseCode::from(kick_code), WS_KICK_REASON);
    } else if send_close_on_exit && !state.kicked.load(Ordering::Acquire) {
        send_close_and_drain(&mut ws, CloseCode::Normal, "stream ended");
    } else {
        let _ = ws.get_mut().inner_mut().shutdown(Shutdown::Both);
    }
}

fn drain_incoming_ws(ws: &mut WebSocket<PrefixedStream>, state: &WsClientState) -> bool {
    if ws.get_mut().inner_mut().set_nonblocking(true).is_err() {
        return false;
    }

    let mut pending_pongs: Vec<String> = Vec::new();
    let mut need_flush = false;
    let mut keep_running = true;

    loop {
        match ws.read() {
            Ok(Message::Text(txt)) => {
                if let Ok(msg) = serde_json::from_str::<WsClientMessage>(&txt) {
                    match msg {
                        WsClientMessage::Ping { t } => {
                            pending_pongs.push(json!({ "type": "pong", "t": t }).to_string());
                        }
                        WsClientMessage::Stats { rtt_ms } => {
                            if let Some(rtt) = rtt_ms.filter(|v| v.is_finite() && *v >= 0.0) {
                                *lock_or_recover(&state.rtt_ms) = Some(rtt);
                            }
                        }
                    }
                }
            }
            Ok(Message::Ping(_)) => {
                need_flush = true;
            }
            Ok(Message::Pong(_)) | Ok(Message::Binary(_)) | Ok(Message::Frame(_)) => {}
            Ok(Message::Close(_)) => {
                let _ = ws.get_mut().inner_mut().set_nonblocking(false);
                let _ = ws.flush();
                keep_running = false;
                break;
            }
            Err(WsError::Io(ref e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(_) => {
                keep_running = false;
                break;
            }
        }
    }

    if ws.get_mut().inner_mut().set_nonblocking(false).is_err() {
        return false;
    }
    let _ = ws
        .get_mut()
        .inner_mut()
        .set_write_timeout(Some(WS_WRITE_TIMEOUT));

    if !keep_running {
        return false;
    }

    if need_flush && ws.flush().is_err() {
        return false;
    }

    for pong_json in pending_pongs {
        let len = pong_json.len() as u64;
        if ws.send(Message::text(pong_json)).is_err() {
            return false;
        }
        state.bytes_sent.fetch_add(len, Ordering::Relaxed);
    }

    true
}

fn send_broadcast_frame(
    ws: &mut WebSocket<PrefixedStream>,
    state: &WsClientState,
    frame: &BroadcastFrame,
    last_seq: &mut Option<u64>,
    last_pts_dur: &mut Option<(u64, u32)>,
) -> bool {
    let seq_gap = last_seq.is_some_and(|prev| frame.seq != prev.wrapping_add(1));
    let pts_gap = last_pts_dur.is_some_and(|(prev_pts, prev_dur)| {
        frame.pts_samples != prev_pts.saturating_add(u64::from(prev_dur))
    });

    let mut flags = WS_FLAG_PCM16LE_48K_STEREO;
    if seq_gap || pts_gap {
        flags |= WS_FLAG_DISCONTINUITY;
    }

    *last_seq = Some(frame.seq);
    *last_pts_dur = Some((frame.pts_samples, frame.duration_samples));

    let wire_seq = (frame.seq & u64::from(u32::MAX)) as u32;
    let payload = encode_ws_audio_frame(wire_seq, frame.pts_samples, flags, &frame.data);
    let payload_len = payload.len() as u64;

    if ws.send(Message::binary(payload)).is_err() {
        state.local_dropped_frames.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    state.bytes_sent.fetch_add(payload_len, Ordering::Relaxed);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::net::{IpAddr, Ipv4Addr, TcpListener};

    use crate::transport::wire::AudioParams;
    use crate::transport::CaptureEndpoint;
    use crate::web::{CaptureFactory, WebBroadcastConfig, WebBroadcastManager};

    fn dummy_capture_factory() -> CaptureFactory {
        Arc::new(|| {
            #[cfg(feature = "gstreamer")]
            let endpoint = CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            };
            #[cfg(not(feature = "gstreamer"))]
            let endpoint = CaptureEndpoint::TestSineWave { freq_hz: 440.0 };

            Ok(crate::web::WebCaptureTarget {
                endpoint,
                audio_params: AudioParams::default(),
                label: "ws-test-sine".into(),
            })
        })
    }

    fn start_test_runtime(require_token: bool) -> (WebBroadcastManager, Arc<BroadcastRuntime>) {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            require_token,
            max_clients: 4,
            idle_stop_grace: Duration::from_millis(100),
            ..Default::default()
        };
        mgr.start(cfg, dummy_capture_factory())
            .expect("manager start");
        let rt = mgr.runtime().expect("runtime");
        (mgr, rt)
    }

    fn accept_http_upgrade_on_listener(listener: &TcpListener) -> (WsUpgradeRequest, bool) {
        let (stream, remote_addr) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream);
        let mut key = String::new();
        let mut subprotocol_token = None;
        let mut has_soundhop_proto = false;

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read header line");
            assert!(n > 0, "unexpected EOF reading upgrade headers");
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, val)) = trimmed.split_once(':') {
                let val = val.trim();
                if name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
                    key = val.to_string();
                } else if name.eq_ignore_ascii_case("Sec-WebSocket-Protocol") {
                    for proto in val.split(',').map(str::trim) {
                        if proto == WS_SUBPROTOCOL {
                            has_soundhop_proto = true;
                        } else if let Some(tok) = proto.strip_prefix("auth.") {
                            subprotocol_token = Some(tok.to_string());
                        }
                    }
                }
            }
        }

        let leftover_bytes = reader.buffer().to_vec();
        let stream = reader.into_inner();
        let req = WsUpgradeRequest {
            stream,
            remote_addr,
            user_agent: "ws-test-agent".into(),
            sec_websocket_key: key,
            subprotocol_token,
            client_sent_soundhop_protocol: has_soundhop_proto,
            leftover_bytes,
        };
        (req, has_soundhop_proto)
    }

    fn connect_ws_client(
        addr: SocketAddr,
        protocols: Option<&str>,
    ) -> (
        WebSocket<TcpStream>,
        tungstenite::handshake::client::Response,
    ) {
        let tcp = TcpStream::connect(addr).expect("connect");
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(4)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(4)));
        let mut builder = tungstenite::http::Request::builder()
            .uri(format!("ws://{addr}/ws"))
            .header("Host", addr.to_string())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            );
        if let Some(proto) = protocols {
            builder = builder.header("Sec-WebSocket-Protocol", proto);
        }
        let req = builder.body(()).expect("build request");
        tungstenite::client(req, tcp).expect("ws client handshake")
    }

    #[test]
    fn binary_header_encoding_matches_spec() {
        let payload = vec![0xABu8; 3840];
        let frame = encode_ws_audio_frame(
            0x0102_0304,
            0x1122_3344_5566_7788,
            WS_FLAG_PCM16LE_48K_STEREO | WS_FLAG_DISCONTINUITY,
            &payload,
        );
        assert_eq!(frame.len(), WS_HEADER_LEN + 3840);
        assert_eq!(&frame[0..2], &WS_FRAME_MAGIC);
        assert_eq!(frame[2], WS_FRAME_VERSION);
        assert_eq!(frame[3], 0x03);
        assert_eq!(&frame[4..8], &0x0102_0304u32.to_le_bytes());
        assert_eq!(&frame[8..16], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&frame[16..], payload.as_slice());
    }

    #[test]
    fn bad_subprotocol_token_sends_1008_policy_violation() {
        let (mgr, rt) = start_test_runtime(true);
        let ws_fanout = WsFanoutHandle::start(WsFanoutConfig {
            runtime: Arc::clone(&rt),
        })
        .expect("ws start");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client_handle = thread::spawn(move || {
            let (mut client_ws, resp) =
                connect_ws_client(addr, Some("soundhop.v1, auth.wrong-token"));
            assert_eq!(
                resp.headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|v| v.to_str().ok()),
                Some(WS_SUBPROTOCOL)
            );
            loop {
                match client_ws.read() {
                    Ok(Message::Close(Some(cf))) => break cf,
                    Ok(_) => continue,
                    Err(err) => panic!("expected Close 1008, got {err}"),
                }
            }
        });

        let (req, _) = accept_http_upgrade_on_listener(&listener);
        let err = ws_fanout
            .accept_upgraded(req)
            .expect_err("bad token must fail");
        assert!(matches!(err, WebError::Unauthorized(_)));
        assert_eq!(ws_fanout.client_count(), 0);

        let close_frame = client_handle.join().expect("client join");
        assert_eq!(close_frame.code, CloseCode::Policy);

        ws_fanout.stop();
        mgr.stop();
    }

    #[test]
    fn auth_timeout_and_wrong_first_frame_send_1008_close() {
        let (mgr, rt) = start_test_runtime(true);
        let ws_fanout = WsFanoutHandle::start(WsFanoutConfig {
            runtime: Arc::clone(&rt),
        })
        .expect("ws start");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Case 1: 2s timeout without sending auth frame
        let client_timeout = thread::spawn(move || {
            let (mut client_ws, _) = connect_ws_client(addr, Some(WS_SUBPROTOCOL));
            loop {
                match client_ws.read() {
                    Ok(Message::Close(Some(cf))) => break cf,
                    Ok(_) => continue,
                    Err(err) => panic!("expected Close 1008 on timeout, got {err}"),
                }
            }
        });

        let (req, _) = accept_http_upgrade_on_listener(&listener);
        let start = Instant::now();
        let err = ws_fanout
            .accept_upgraded(req)
            .expect_err("timeout must fail");
        assert!(matches!(err, WebError::Unauthorized(_)));
        assert!(start.elapsed() >= Duration::from_millis(1800));
        let close_frame = client_timeout.join().expect("timeout client join");
        assert_eq!(close_frame.code, CloseCode::Policy);

        // Case 2: wrong first frame (non-auth JSON / binary)
        let client_wrong = thread::spawn(move || {
            let (mut client_ws, _) = connect_ws_client(addr, Some(WS_SUBPROTOCOL));
            client_ws
                .send(Message::text(r#"{"type":"ping","t":1}"#))
                .expect("send wrong frame");
            loop {
                match client_ws.read() {
                    Ok(Message::Close(Some(cf))) => break cf,
                    Ok(_) => continue,
                    Err(err) => panic!("expected Close 1008 on wrong frame, got {err}"),
                }
            }
        });

        let (req, _) = accept_http_upgrade_on_listener(&listener);
        let err = ws_fanout
            .accept_upgraded(req)
            .expect_err("wrong first frame must fail");
        assert!(matches!(err, WebError::Unauthorized(_)));
        let close_frame = client_wrong.join().expect("wrong client join");
        assert_eq!(close_frame.code, CloseCode::Policy);

        ws_fanout.stop();
        mgr.stop();
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn subprotocol_auth_ping_pong_stats_and_kick() {
        let (mgr, rt) = start_test_runtime(true);
        let tok = rt.current_token().expect("token");
        let ws_fanout = WsFanoutHandle::start(WsFanoutConfig {
            runtime: Arc::clone(&rt),
        })
        .expect("ws start");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client_handle = thread::spawn(move || {
            let proto = format!("{WS_SUBPROTOCOL}, auth.{tok}");
            let (mut client_ws, resp) = connect_ws_client(addr, Some(&proto));
            assert_eq!(
                resp.headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|v| v.to_str().ok()),
                Some(WS_SUBPROTOCOL)
            );

            client_ws
                .send(Message::text(r#"{"type":"ping","t":98765}"#))
                .expect("send ping");
            client_ws
                .send(Message::text(r#"{"type":"stats","rttMs":12.5}"#))
                .expect("send stats");

            loop {
                match client_ws.read() {
                    Ok(Message::Text(txt)) => {
                        let v: Value = serde_json::from_str(&txt).expect("valid pong json");
                        assert_eq!(v["type"], "pong");
                        assert_eq!(v["t"], 98765);
                        break;
                    }
                    Ok(Message::Binary(_)) => continue,
                    Ok(other) => panic!("unexpected ws message: {other:?}"),
                    Err(err) => panic!("read failed: {err}"),
                }
            }
            client_ws
        });

        let (req, _) = accept_http_upgrade_on_listener(&listener);
        let client_id = ws_fanout.accept_upgraded(req).expect("accept");
        assert_eq!(ws_fanout.client_count(), 1);

        let mut client_ws = client_handle.join().expect("client ping/pong");

        // Wait briefly for stats frame to be processed.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut seen_rtt = None;
        while Instant::now() < deadline {
            let list = ws_fanout.list_clients();
            if let Some(info) = list.first() {
                if info.rtt_ms.is_some() {
                    seen_rtt = info.rtt_ms;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(seen_rtt, Some(12.5));

        assert!(ws_fanout.kick_client(&client_id));
        assert!(!ws_fanout.kick_client(&client_id));
        assert_eq!(ws_fanout.client_count(), 0);

        // A kicked client receives a distinct terminal close code so the browser
        // does not treat it as a network drop and auto-reconnect.
        let close_watch = thread::spawn(move || loop {
            match client_ws.read() {
                Ok(Message::Close(Some(frame))) => break frame,
                Ok(_) => continue,
                Err(err) => panic!("expected kick close frame, got {err}"),
            }
        });
        let close_frame = close_watch.join().expect("kick close frame");
        assert_eq!(close_frame.code, CloseCode::from(WS_KICK_CLOSE_CODE));
        assert_eq!(close_frame.reason, WS_KICK_REASON);

        ws_fanout.stop();
        mgr.stop();
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn first_frame_json_auth_with_leftover_bytes_and_pcm_streaming() {
        let (mgr, rt) = start_test_runtime(true);
        let tok = rt.current_token().expect("token");
        let ws_fanout = WsFanoutHandle::start(WsFanoutConfig {
            runtime: Arc::clone(&rt),
        })
        .expect("ws start");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Connect manually using raw HTTP upgrade + pipelined WS auth frame so leftover_bytes
        // through PrefixedStream is exercised directly.
        let mut raw_client = TcpStream::connect(addr).expect("connect");
        raw_client
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let ws_key = tungstenite::handshake::client::generate_key();
        let upgrade_req = format!(
            "GET /ws HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {ws_key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Protocol: {WS_SUBPROTOCOL}\r\n\r\n"
        );
        raw_client.write_all(upgrade_req.as_bytes()).unwrap();
        raw_client.flush().unwrap();

        let (mut req, _) = accept_http_upgrade_on_listener(&listener);

        // Build a masked client WS text frame for {"type":"auth","token":tok} and inject it
        // into req.leftover_bytes to verify PrefixedStream replays buffered bytes before TcpStream.
        let auth_json = json!({ "type": "auth", "token": tok }).to_string();
        let mut frame_bytes = Vec::new();
        frame_bytes.push(0x81); // FIN + Text
        let mask = [0x11, 0x22, 0x33, 0x44];
        let payload = auth_json.as_bytes();
        assert!(payload.len() < 126);
        frame_bytes.push(0x80 | (payload.len() as u8)); // MASK bit + len
        frame_bytes.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame_bytes.push(b ^ mask[i % 4]);
        }
        req.leftover_bytes = frame_bytes;

        let client_id = ws_fanout.accept_upgraded(req).expect("first-frame auth");

        // Read the server's 101 Switching Protocols response on raw_client and wrap as client WS.
        let mut reader = BufReader::new(raw_client);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).unwrap();
        assert!(status_line.starts_with("HTTP/1.1 101"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
        }
        let leftover_client = reader.buffer().to_vec();
        let mut client_ws = WebSocket::from_partially_read(
            reader.into_inner(),
            leftover_client,
            Role::Client,
            None,
        );

        // Inject synthetic PCM frames with a sequence gap to verify header encoding & discontinuity.
        let mut wakeups = Vec::new();
        let pcm_a = Arc::<[u8]>::from(vec![0x11u8; 3840]);
        let pcm_b = Arc::<[u8]>::from(vec![0x22u8; 3840]);
        rt.registry().dispatch_pcm(
            Arc::new(BroadcastFrame {
                seq: 1000,
                pts_samples: 0,
                duration_samples: 960,
                captured_at: Instant::now(),
                data: Arc::clone(&pcm_a),
            }),
            &mut wakeups,
        );
        rt.registry().dispatch_pcm(
            Arc::new(BroadcastFrame {
                seq: 1002,
                pts_samples: 1920,
                duration_samples: 960,
                captured_at: Instant::now(),
                data: Arc::clone(&pcm_b),
            }),
            &mut wakeups,
        );

        let mut saw_a = false;
        let mut saw_b = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !(saw_a && saw_b) {
            if let Ok(Message::Binary(bin)) = client_ws.read() {
                if bin.len() == WS_HEADER_LEN + 3840 {
                    assert_eq!(&bin[0..2], &WS_FRAME_MAGIC);
                    assert_eq!(bin[2], WS_FRAME_VERSION);
                    let seq = u32::from_le_bytes(bin[4..8].try_into().unwrap());
                    let pts = u64::from_le_bytes(bin[8..16].try_into().unwrap());
                    if seq == 1000 {
                        assert_eq!(pts, 0);
                        assert_eq!(&bin[16..], pcm_a.as_ref());
                        saw_a = true;
                    } else if seq == 1002 {
                        assert_eq!(bin[3], WS_FLAG_PCM16LE_48K_STEREO | WS_FLAG_DISCONTINUITY);
                        assert_eq!(pts, 1920);
                        assert_eq!(&bin[16..], pcm_b.as_ref());
                        saw_b = true;
                    }
                }
            }
        }
        assert!(
            saw_a && saw_b,
            "did not receive expected synthetic PCM frames"
        );

        let clients = ws_fanout.list_clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, client_id);
        assert!(clients[0].bytes_sent >= ((WS_HEADER_LEN + 3840) * 2) as u64);

        ws_fanout.stop();
        mgr.stop();
    }

    #[cfg(not(feature = "gstreamer"))]
    #[test]
    fn accept_upgraded_returns_service_unavailable_without_gstreamer() {
        let (mgr, rt) = start_test_runtime(false);
        let ws_fanout = WsFanoutHandle::start(WsFanoutConfig {
            runtime: Arc::clone(&rt),
        })
        .expect("ws start");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client_handle = thread::spawn(move || {
            let (mut client_ws, _) = connect_ws_client(addr, Some(WS_SUBPROTOCOL));
            let _ = client_ws.read();
        });

        let (req, _) = accept_http_upgrade_on_listener(&listener);
        let err = ws_fanout
            .accept_upgraded(req)
            .expect_err("should fail without gstreamer");
        assert!(matches!(err, WebError::ServiceUnavailable(_)));
        let _ = client_handle.join();

        ws_fanout.stop();
        mgr.stop();
    }
}
