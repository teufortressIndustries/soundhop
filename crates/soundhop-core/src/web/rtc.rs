//! Single-UDP-port ICE-Lite WebRTC multiplexer (`web-rtc-mux`) backed by `str0m`.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt, Writer};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::transport::{udp_poke_hook, BroadcastFrame, BroadcastSubscription, WakeupHook};
use crate::util::{local_addresses, lock_or_recover, now_secs};

use super::{
    gstreamer_available, BroadcastRuntime, ClientSlotGuard, ListenerGuard, WebClientInfo, WebError,
    WebResult,
};

/// Maximum accepted SDP offer payload size (`64 KiB`).
const MAX_SDP_OFFER_BYTES: usize = 64 * 1024;
/// Capacity of the `web-rtc-mux` control command channel.
const MUX_CMD_CHANNEL_CAPACITY: usize = 64;
/// Default ICE/DTLS handshake deadline before an unestablished peer is evicted.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Periodic stats emission interval configured on each `str0m::Rtc` peer.
const PEER_STATS_INTERVAL: Duration = Duration::from_secs(2);
/// Maximum UDP read timeout when idle so `stop_flag` and deadlines stay responsive.
const MAX_IDLE_POLL_TIMEOUT: Duration = Duration::from_millis(250);
/// Minimum positive UDP socket read timeout (passing `Duration::ZERO` disables timeouts).
const MIN_UDP_READ_TIMEOUT: Duration = Duration::from_millis(1);
/// UDP receive buffer size matching `str0m`'s datagram MTU headroom.
const UDP_RECV_BUF_BYTES: usize = 2048;
/// STUN magic cookie (RFC 5389 §6).
const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
/// STUN Binding Request message type (class=request, method=binding).
const STUN_BINDING_REQUEST: u16 = 0x0001;
/// STUN `USERNAME` attribute type (RFC 5389 §15.3).
const STUN_ATTR_USERNAME: u16 = 0x0006;

/// Configuration for starting the `web-rtc-mux` thread.
#[derive(Clone)]
pub struct RtcMuxConfig {
    pub bind_ip: IpAddr,
    pub udp_port: u16,
    pub runtime: Arc<BroadcastRuntime>,
}

/// Incoming browser SDP offer passed from HTTP `POST /webrtc/offer` to `web-rtc-mux`.
#[derive(Debug, Clone)]
pub struct RtcOfferRequest {
    pub offer_sdp: String,
    pub remote_addr: SocketAddr,
    pub user_agent: String,
    pub client_name: Option<String>,
}

/// Answer payload returned by `web-rtc-mux` after accepting a browser SDP offer.
#[derive(Debug, Clone)]
pub struct RtcOfferResponse {
    pub peer_id: String,
    pub answer_sdp: String,
    pub ice_lite: bool,
}

enum MuxCommand {
    AddPeer {
        peer: Box<PeerRtc>,
        initial_transmits: Vec<Transmit>,
        reply_tx: SyncSender<WebResult<()>>,
    },
    RemovePeer {
        peer_id: String,
        send_close: bool,
        reply_tx: SyncSender<bool>,
    },
    ListClients {
        reply_tx: SyncSender<Vec<WebClientInfo>>,
    },
    Stop,
}

struct PeerRtc {
    id: String,
    rtc: Rtc,
    local_ufrag: String,
    audio_mid: Option<Mid>,
    opus_pt: Option<Pt>,
    ice_connected: bool,
    connected_at: Option<Instant>,
    handshake_deadline: Instant,
    next_timeout: Instant,
    sample_idx: u64,
    remote_addr: String,
    user_agent: String,
    connected_since: f64,
    bytes_sent: u64,
    rtt_ms: Option<f64>,
    dropped_frames: u32,
    _slot_guard: ClientSlotGuard,
    _listener_guard: ListenerGuard,
}

impl PeerRtc {
    fn to_client_info(&self) -> WebClientInfo {
        WebClientInfo {
            id: self.id.clone(),
            remote_addr: self.remote_addr.clone(),
            transport: "webrtc".to_string(),
            user_agent: self.user_agent.clone(),
            connected_since: self.connected_since,
            bytes_sent: self.bytes_sent,
            rtt_ms: self.rtt_ms,
            dropped_frames: self.dropped_frames,
        }
    }

    fn is_handshake_complete(&self) -> bool {
        self.ice_connected && self.rtc.is_connected()
    }
}

/// Handle to the running `web-rtc-mux` thread.
pub struct RtcMuxHandle {
    local_port: u16,
    bind_ip: IpAddr,
    runtime: Arc<BroadcastRuntime>,
    stopped: Arc<AtomicBool>,
    next_peer_seq: AtomicU64,
    client_count: Arc<AtomicUsize>,
    clients_snapshot: Arc<RwLock<Vec<WebClientInfo>>>,
    cmd_tx: SyncSender<MuxCommand>,
    poke_socket: UdpSocket,
    poke_target: SocketAddr,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl RtcMuxHandle {
    /// Start the `web-rtc-mux` UDP multiplexer thread and bind `config.bind_ip:config.udp_port`.
    pub fn start(config: RtcMuxConfig) -> WebResult<Self> {
        Self::start_with_handshake_timeout(config, DEFAULT_HANDSHAKE_TIMEOUT)
    }

    fn start_with_handshake_timeout(
        config: RtcMuxConfig,
        handshake_timeout: Duration,
    ) -> WebResult<Self> {
        let udp_socket = UdpSocket::bind((config.bind_ip, config.udp_port))?;
        let local_udp_addr = udp_socket.local_addr()?;
        let local_port = local_udp_addr.port();

        let poke_target = if local_udp_addr.ip().is_unspecified() {
            match local_udp_addr {
                SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port),
                SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), local_port),
            }
        } else {
            local_udp_addr
        };

        let poke_bind = if poke_target.is_ipv6() {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)
        } else {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        };
        let poke_socket = UdpSocket::bind(poke_bind)?;
        poke_socket.set_nonblocking(true)?;
        let poke_addr = poke_socket.local_addr()?;

        let opus_poke_hook = udp_poke_hook(poke_target)?;

        let (cmd_tx, cmd_rx) = mpsc::sync_channel(MUX_CMD_CHANNEL_CAPACITY);
        let stopped = Arc::new(AtomicBool::new(false));
        let client_count = Arc::new(AtomicUsize::new(0));
        let clients_snapshot = Arc::new(RwLock::new(Vec::new()));

        let mut mux_state = MuxState {
            runtime: Arc::clone(&config.runtime),
            udp_socket,
            local_udp_addr,
            poke_addr,
            opus_poke_hook,
            opus_sub: None,
            peers: HashMap::new(),
            addr_to_peer: HashMap::new(),
            ufrag_to_peer: HashMap::new(),
            client_count: Arc::clone(&client_count),
            clients_snapshot: Arc::clone(&clients_snapshot),
            stopped: Arc::clone(&stopped),
            handshake_timeout,
        };

        let thread_handle = thread::Builder::new()
            .name("web-rtc-mux".into())
            .spawn(move || mux_state.run_loop(cmd_rx))?;

        Ok(Self {
            local_port,
            bind_ip: config.bind_ip,
            runtime: config.runtime,
            stopped,
            next_peer_seq: AtomicU64::new(1),
            client_count,
            clients_snapshot,
            cmd_tx,
            poke_socket,
            poke_target,
            thread: Mutex::new(Some(thread_handle)),
        })
    }

    /// Actual UDP port bound by `web-rtc-mux`.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Accept a browser SDP offer, allocate a peer slot, and return the ICE-Lite SDP answer.
    pub fn accept_offer(&self, req: RtcOfferRequest) -> WebResult<RtcOfferResponse> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(WebError::NotRunning);
        }
        if !gstreamer_available() {
            return Err(WebError::ServiceUnavailable(
                "GStreamer/Opus support disabled in this build".into(),
            ));
        }

        let trimmed_sdp = req.offer_sdp.trim();
        if trimmed_sdp.is_empty() || req.offer_sdp.len() > MAX_SDP_OFFER_BYTES {
            return Err(WebError::BadRequest(
                "SDP offer is empty or exceeds size limit".into(),
            ));
        }

        // Reserve combined capacity before negotiating. Dropped automatically on any error.
        let slot_guard = self.runtime.try_acquire_client_slot()?;

        let offer = SdpOffer::from_sdp_string(&req.offer_sdp)
            .map_err(|e| WebError::BadRequest(format!("invalid SDP offer: {e}")))?;

        let now = Instant::now();
        let mut rtc = RtcConfig::new()
            .set_ice_lite(true)
            .set_stats_interval(Some(PEER_STATS_INTERVAL))
            .build(now);

        for candidate in build_local_candidates(self.bind_ip, self.local_port) {
            let _ = rtc.add_local_candidate(candidate);
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| WebError::BadRequest(format!("incompatible SDP offer: {e}")))?;

        let (audio_mid, next_timeout, initial_transmits) = drain_initial_rtc(&mut rtc)?;
        let opus_pt = audio_mid.and_then(|mid| rtc.writer(mid).and_then(|w| select_opus_pt(&w)));

        let answer_sdp = answer.to_sdp_string();
        if !answer_has_sendable_opus_audio(&answer_sdp) {
            return Err(WebError::BadRequest(
                "SDP offer does not include a compatible sendable 48 kHz Opus audio section".into(),
            ));
        }

        let local_ufrag = extract_ice_ufrag(&answer_sdp).ok_or_else(|| {
            WebError::Rtc("generated SDP answer is missing local a=ice-ufrag".into())
        })?;

        // Only start the lazy GStreamer broadcast pipeline after SDP validation succeeds.
        let listener_guard = self.runtime.acquire_listener()?;

        let peer_id = format!("rtc-{}", self.next_peer_seq.fetch_add(1, Ordering::Relaxed));
        let label = req
            .client_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(req.user_agent);

        let peer = Box::new(PeerRtc {
            id: peer_id.clone(),
            rtc,
            local_ufrag,
            audio_mid,
            opus_pt,
            ice_connected: false,
            connected_at: None,
            handshake_deadline: now + DEFAULT_HANDSHAKE_TIMEOUT,
            next_timeout,
            sample_idx: 0,
            remote_addr: req.remote_addr.to_string(),
            user_agent: label,
            connected_since: now_secs(),
            bytes_sent: 0,
            rtt_ms: None,
            dropped_frames: 0,
            _slot_guard: slot_guard,
            _listener_guard: listener_guard,
        });

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send_cmd(MuxCommand::AddPeer {
            peer,
            initial_transmits,
            reply_tx,
        })?;
        reply_rx.recv().map_err(|_| WebError::ChannelClosed)??;

        Ok(RtcOfferResponse {
            peer_id,
            answer_sdp,
            ice_lite: true,
        })
    }

    /// Remove a peer (for example on `DELETE /webrtc/peer/:id`). Returns `true` if found.
    pub fn remove_peer(&self, peer_id: &str) -> bool {
        self.remove_peer_internal(peer_id, true)
    }

    /// Forcefully disconnect and evict a peer by ID. Returns `true` if found.
    pub fn kick_peer(&self, peer_id: &str) -> bool {
        self.remove_peer_internal(peer_id, true)
    }

    /// Snapshot currently active WebRTC peers.
    pub fn list_clients(&self) -> Vec<WebClientInfo> {
        if !self.stopped.load(Ordering::Acquire) {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            if self.send_cmd(MuxCommand::ListClients { reply_tx }).is_ok() {
                if let Ok(clients) = reply_rx.recv_timeout(Duration::from_millis(250)) {
                    return clients;
                }
            }
        }
        self.clients_snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Current number of active WebRTC peers.
    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::Acquire)
    }

    /// Stop the `web-rtc-mux` thread and drop all active peers.
    pub fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            let _ = self.cmd_tx.try_send(MuxCommand::Stop);
            self.poke();
        }
        if let Some(handle) = lock_or_recover(&self.thread).take() {
            let _ = handle.join();
        }
    }

    fn remove_peer_internal(&self, peer_id: &str, send_close: bool) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .send_cmd(MuxCommand::RemovePeer {
                peer_id: peer_id.to_string(),
                send_close,
                reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    fn send_cmd(&self, cmd: MuxCommand) -> WebResult<()> {
        self.cmd_tx.send(cmd).map_err(|_| WebError::ChannelClosed)?;
        self.poke();
        Ok(())
    }

    fn poke(&self) {
        let _ = self.poke_socket.send_to(&[0u8], self.poke_target);
    }
}

impl Drop for RtcMuxHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct MuxState {
    runtime: Arc<BroadcastRuntime>,
    udp_socket: UdpSocket,
    local_udp_addr: SocketAddr,
    poke_addr: SocketAddr,
    opus_poke_hook: WakeupHook,
    opus_sub: Option<BroadcastSubscription>,
    peers: HashMap<String, PeerRtc>,
    addr_to_peer: HashMap<SocketAddr, String>,
    ufrag_to_peer: HashMap<String, String>,
    client_count: Arc<AtomicUsize>,
    clients_snapshot: Arc<RwLock<Vec<WebClientInfo>>>,
    stopped: Arc<AtomicBool>,
    handshake_timeout: Duration,
}

impl MuxState {
    fn run_loop(&mut self, cmd_rx: Receiver<MuxCommand>) {
        let mut buf = vec![0u8; UDP_RECV_BUF_BYTES];
        let mut snapshot_dirty = false;
        let mut last_snapshot_sync = Instant::now();

        loop {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }

            let mut state_changed = self.drain_commands(&cmd_rx);
            if self.stopped.load(Ordering::Acquire) {
                break;
            }

            if self.drain_opus_frames() {
                snapshot_dirty = true;
            }

            if self.drive_due_timeouts(Instant::now()) {
                state_changed = true;
            }

            if self.evict_expired_or_dead(Instant::now()) {
                state_changed = true;
            }

            if state_changed
                || (snapshot_dirty && last_snapshot_sync.elapsed() >= Duration::from_millis(250))
            {
                self.sync_snapshot();
                snapshot_dirty = false;
                last_snapshot_sync = Instant::now();
            }

            let wait_dur = self.compute_read_timeout(Instant::now());
            let _ = self.udp_socket.set_read_timeout(Some(wait_dur));

            buf.resize(UDP_RECV_BUF_BYTES, 0);
            match self.udp_socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    if source == self.poke_addr {
                        // Control wakeup poke; drain channels and frames on next iteration.
                        continue;
                    }
                    let recv_time = Instant::now();
                    if self.handle_udp_packet(recv_time, source, &buf[..n]) {
                        snapshot_dirty = true;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    if self.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }

        self.shutdown();
    }

    fn drain_commands(&mut self, cmd_rx: &Receiver<MuxCommand>) -> bool {
        let mut changed = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(MuxCommand::AddPeer {
                    mut peer,
                    initial_transmits,
                    reply_tx,
                }) => {
                    peer.handshake_deadline = Instant::now() + self.handshake_timeout;
                    for tx in initial_transmits {
                        if self
                            .udp_socket
                            .send_to(&tx.contents, tx.destination)
                            .is_ok()
                        {
                            peer.bytes_sent =
                                peer.bytes_sent.saturating_add(tx.contents.len() as u64);
                        }
                    }
                    let id = peer.id.clone();
                    let ufrag = peer.local_ufrag.clone();
                    self.ufrag_to_peer.insert(ufrag, id.clone());
                    self.peers.insert(id, *peer);
                    self.ensure_opus_subscription();
                    changed = true;
                    let _ = reply_tx.send(Ok(()));
                }
                Ok(MuxCommand::RemovePeer {
                    peer_id,
                    send_close,
                    reply_tx,
                }) => {
                    let removed = self.remove_peer_internal(&peer_id, send_close);
                    if removed {
                        changed = true;
                    }
                    let _ = reply_tx.send(removed);
                }
                Ok(MuxCommand::ListClients { reply_tx }) => {
                    let _ = reply_tx.send(self.collect_clients());
                }
                Ok(MuxCommand::Stop) | Err(TryRecvError::Disconnected) => {
                    self.stopped.store(true, Ordering::Release);
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        changed
    }

    fn drain_opus_frames(&mut self) -> bool {
        if self.peers.is_empty() {
            self.opus_sub = None;
            return false;
        }
        if self.opus_sub.is_none() && self.runtime.is_pipeline_running() {
            self.ensure_opus_subscription();
        }
        let Some(sub) = self.opus_sub.take() else {
            return false;
        };

        let mut sent_any = false;
        let mut disconnected = false;
        loop {
            match sub.try_recv() {
                Ok(frame) => {
                    if self.broadcast_opus_frame(&frame) {
                        sent_any = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if disconnected {
            if !self.peers.is_empty() && self.runtime.is_pipeline_running() {
                self.ensure_opus_subscription();
            }
        } else if !self.peers.is_empty() {
            self.opus_sub = Some(sub);
        }

        sent_any
    }

    fn broadcast_opus_frame(&mut self, frame: &Arc<BroadcastFrame>) -> bool {
        let mut any_sent = false;
        let mut failed_peers = Vec::new();
        let mut peer_ids: Vec<String> = self.peers.keys().cloned().collect();
        peer_ids.sort_unstable();

        for peer_id in peer_ids {
            let Some(peer) = self.peers.get_mut(&peer_id) else {
                continue;
            };
            if !peer.is_handshake_complete() {
                continue;
            }
            let (Some(mid), Some(pt)) = (peer.audio_mid, peer.opus_pt) else {
                continue;
            };
            let Some(writer) = peer.rtc.writer(mid) else {
                continue;
            };

            let rtp_time = MediaTime::new(peer.sample_idx, Frequency::FORTY_EIGHT_KHZ);
            peer.sample_idx = peer
                .sample_idx
                .saturating_add(u64::from(frame.duration_samples));

            if writer
                .write(pt, frame.captured_at, rtp_time, Arc::clone(&frame.data))
                .is_err()
            {
                peer.dropped_frames = peer.dropped_frames.saturating_add(1);
            }

            let keep_alive = drain_peer_output(peer, &self.udp_socket);
            if keep_alive {
                any_sent = true;
            } else {
                failed_peers.push(peer_id);
            }
        }

        for peer_id in failed_peers {
            self.remove_peer_internal(&peer_id, false);
        }

        any_sent
    }

    fn drive_due_timeouts(&mut self, now: Instant) -> bool {
        let due_ids: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, peer)| peer.next_timeout <= now)
            .map(|(id, _)| id.clone())
            .collect();

        let mut removed_any = false;
        for peer_id in due_ids {
            let Some(peer) = self.peers.get_mut(&peer_id) else {
                continue;
            };
            if peer.rtc.handle_input(Input::Timeout(now)).is_err() {
                self.remove_peer_internal(&peer_id, false);
                removed_any = true;
                continue;
            }
            if !drain_peer_output(peer, &self.udp_socket) {
                self.remove_peer_internal(&peer_id, false);
                removed_any = true;
            }
        }
        removed_any
    }

    fn evict_expired_or_dead(&mut self, now: Instant) -> bool {
        let to_evict: Vec<String> = self
            .peers
            .iter()
            .filter_map(|(id, peer)| {
                if !peer.rtc.is_alive()
                    || (!peer.is_handshake_complete() && now >= peer.handshake_deadline)
                {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        let mut changed = false;
        for peer_id in to_evict {
            if self.remove_peer_internal(&peer_id, false) {
                changed = true;
            }
        }
        changed
    }

    fn handle_udp_packet(&mut self, recv_time: Instant, source: SocketAddr, pkt: &[u8]) -> bool {
        let peer_id = if let Some(existing) = self.addr_to_peer.get(&source) {
            existing.clone()
        } else if let Some(local_ufrag) = extract_stun_binding_local_ufrag(pkt) {
            let Some(matched_id) = self.ufrag_to_peer.get(local_ufrag).cloned() else {
                return false;
            };
            self.addr_to_peer.insert(source, matched_id.clone());
            matched_id
        } else {
            // Drop unrecognized non-STUN packets from unknown remote addresses.
            return false;
        };

        let Ok(receive) = Receive::new(Protocol::Udp, source, self.local_udp_addr, pkt) else {
            return false;
        };
        let input = Input::Receive(recv_time, receive);

        let Some(peer) = self.peers.get_mut(&peer_id) else {
            self.addr_to_peer.remove(&source);
            return false;
        };

        if !peer.rtc.accepts(&input) {
            return false;
        }

        if peer.rtc.handle_input(input).is_err() {
            self.remove_peer_internal(&peer_id, false);
            return true;
        }

        if !drain_peer_output(peer, &self.udp_socket) {
            self.remove_peer_internal(&peer_id, false);
            return true;
        }

        true
    }

    fn compute_read_timeout(&self, now: Instant) -> Duration {
        let mut earliest: Option<Instant> = None;
        for peer in self.peers.values() {
            earliest = Some(match earliest {
                Some(cur) => cur.min(peer.next_timeout),
                None => peer.next_timeout,
            });
            if !peer.is_handshake_complete() {
                earliest = Some(match earliest {
                    Some(cur) => cur.min(peer.handshake_deadline),
                    None => peer.handshake_deadline,
                });
            }
        }

        earliest
            .map(|dl| {
                dl.saturating_duration_since(now)
                    .min(MAX_IDLE_POLL_TIMEOUT)
                    .max(MIN_UDP_READ_TIMEOUT)
            })
            .unwrap_or(MAX_IDLE_POLL_TIMEOUT)
    }

    fn remove_peer_internal(&mut self, peer_id: &str, send_close: bool) -> bool {
        let Some(mut peer) = self.peers.remove(peer_id) else {
            return false;
        };
        if send_close {
            let _ = peer.rtc.close();
            let _ = drain_peer_output(&mut peer, &self.udp_socket);
        }
        self.addr_to_peer.retain(|_, id| id != peer_id);
        self.ufrag_to_peer.retain(|_, id| id != peer_id);
        if self.peers.is_empty() {
            self.opus_sub = None;
        }
        true
    }

    fn ensure_opus_subscription(&mut self) {
        if self.opus_sub.is_none() && !self.peers.is_empty() {
            let sub = self
                .runtime
                .registry()
                .subscribe_opus(Some(Arc::clone(&self.opus_poke_hook)));
            self.opus_sub = Some(sub);
        }
    }

    fn collect_clients(&self) -> Vec<WebClientInfo> {
        let mut clients: Vec<WebClientInfo> =
            self.peers.values().map(PeerRtc::to_client_info).collect();
        clients.sort_by(|a, b| {
            a.connected_since
                .partial_cmp(&b.connected_since)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        clients
    }

    fn sync_snapshot(&self) {
        let clients = self.collect_clients();
        self.client_count.store(clients.len(), Ordering::Release);
        *self
            .clients_snapshot
            .write()
            .unwrap_or_else(|e| e.into_inner()) = clients;
    }

    fn shutdown(&mut self) {
        let peer_ids: Vec<String> = self.peers.keys().cloned().collect();
        for peer_id in peer_ids {
            self.remove_peer_internal(&peer_id, true);
        }
        self.opus_sub = None;
        self.addr_to_peer.clear();
        self.ufrag_to_peer.clear();
        self.sync_snapshot();
    }
}

fn build_local_candidates(bind_ip: IpAddr, udp_port: u16) -> Vec<Candidate> {
    let mut addrs: Vec<IpAddr> = Vec::new();
    if bind_ip.is_unspecified() {
        for ip_str in local_addresses(false) {
            if let Ok(IpAddr::V4(v4)) = ip_str.parse::<IpAddr>() {
                let ip = IpAddr::V4(v4);
                if !addrs.contains(&ip) {
                    addrs.push(ip);
                }
            }
        }
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        if !addrs.contains(&loopback) {
            addrs.push(loopback);
        }
    } else if bind_ip.is_loopback() {
        addrs.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
        if bind_ip.is_ipv6() {
            addrs.push(bind_ip);
        }
    } else {
        addrs.push(bind_ip);
    }

    addrs
        .into_iter()
        .filter_map(|ip| Candidate::host(SocketAddr::new(ip, udp_port), "udp").ok())
        .collect()
}

fn select_opus_pt(writer: &Writer<'_>) -> Option<Pt> {
    writer
        .payload_params()
        .find(|p| {
            let spec = p.spec();
            spec.codec == Codec::Opus && spec.clock_rate == Frequency::FORTY_EIGHT_KHZ
        })
        .map(|p| p.pt())
}

fn drain_initial_rtc(rtc: &mut Rtc) -> WebResult<(Option<Mid>, Instant, Vec<Transmit>)> {
    let mut audio_mid = None;
    let mut transmits = Vec::new();

    let next_timeout = loop {
        match rtc
            .poll_output()
            .map_err(|e| WebError::Rtc(format!("poll_output after accept_offer failed: {e}")))?
        {
            Output::Timeout(t) => break t,
            Output::Transmit(tx) => transmits.push(tx),
            Output::Event(Event::MediaAdded(media)) => {
                if media.kind == MediaKind::Audio && media.direction.is_sending() {
                    audio_mid.get_or_insert(media.mid);
                }
            }
            Output::Event(_) => {}
        }
    };

    Ok((audio_mid, next_timeout, transmits))
}

fn drain_peer_output(peer: &mut PeerRtc, udp_socket: &UdpSocket) -> bool {
    let mut keep_alive = true;

    loop {
        match peer.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                peer.next_timeout = t;
                break;
            }
            Ok(Output::Transmit(tx)) => {
                if let Ok(sent) = udp_socket.send_to(&tx.contents, tx.destination) {
                    peer.bytes_sent = peer.bytes_sent.saturating_add(sent as u64);
                }
            }
            Ok(Output::Event(ev)) => match ev {
                Event::MediaAdded(media) => {
                    if media.kind == MediaKind::Audio && media.direction.is_sending() {
                        peer.audio_mid = Some(media.mid);
                        if let Some(writer) = peer.rtc.writer(media.mid) {
                            peer.opus_pt = select_opus_pt(&writer);
                        }
                    }
                }
                Event::MediaChanged(changed) => {
                    if peer.audio_mid == Some(changed.mid) && !changed.direction.is_sending() {
                        peer.audio_mid = None;
                    }
                }
                Event::IceConnectionStateChange(state) => match state {
                    IceConnectionState::Connected | IceConnectionState::Completed => {
                        peer.ice_connected = true;
                        peer.connected_at.get_or_insert_with(Instant::now);
                    }
                    IceConnectionState::Disconnected => {
                        peer.ice_connected = false;
                        keep_alive = false;
                    }
                    IceConnectionState::New | IceConnectionState::Checking => {}
                },
                Event::Connected => {
                    peer.ice_connected = true;
                    peer.connected_at.get_or_insert_with(Instant::now);
                }
                Event::PeerStats(stats) => {
                    let rtt = stats.rtt.or_else(|| {
                        stats
                            .selected_candidate_pair
                            .as_ref()
                            .and_then(|pair| pair.current_round_trip_time)
                    });
                    if let Some(rtt) = rtt {
                        peer.rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
                    }
                }
                Event::Closed => {
                    keep_alive = false;
                }
                _ => {}
            },
            Err(_) => {
                keep_alive = false;
                break;
            }
        }
    }

    keep_alive && peer.rtc.is_alive()
}

/// Extract the first `a=ice-ufrag:<value>` attribute from an SDP document.
pub(crate) fn extract_ice_ufrag(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|line| {
        line.trim()
            .strip_prefix("a=ice-ufrag:")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

fn answer_has_sendable_opus_audio(answer_sdp: &str) -> bool {
    let mut in_audio = false;
    let mut active_port = false;
    let mut has_send_dir = false;
    let mut has_opus_48k = false;

    let check_section =
        |in_audio: bool, active_port: bool, has_send_dir: bool, has_opus_48k: bool| {
            in_audio && active_port && has_send_dir && has_opus_48k
        };

    for raw_line in answer_sdp.lines() {
        let line = raw_line.trim();
        if let Some(m_rest) = line.strip_prefix("m=") {
            if check_section(in_audio, active_port, has_send_dir, has_opus_48k) {
                return true;
            }
            in_audio = m_rest.starts_with("audio ");
            active_port = if in_audio {
                m_rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|p| p.parse::<u16>().ok())
                    .is_some_and(|p| p > 0)
            } else {
                false
            };
            // Per RFC 3264, omitted direction defaults to sendrecv; str0m emits explicit attrs.
            has_send_dir = in_audio;
            has_opus_48k = false;
            continue;
        }
        if !in_audio {
            continue;
        }
        if line == "a=recvonly" || line == "a=inactive" {
            has_send_dir = false;
        } else if line == "a=sendonly" || line == "a=sendrecv" {
            has_send_dir = true;
        } else if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some((_, codec_part)) = rtpmap.split_once(' ') {
                if codec_part.to_ascii_lowercase().starts_with("opus/48000") {
                    has_opus_48k = true;
                }
            }
        }
    }

    check_section(in_audio, active_port, has_send_dir, has_opus_48k)
}

/// Extract the local ICE `ufrag` from a STUN Binding Request's `USERNAME` (`0x0006`) attribute.
///
/// STUN `USERNAME` is formatted as `"<local_ufrag>:<remote_ufrag>"` per RFC 8445.
pub(crate) fn extract_stun_binding_local_ufrag(pkt: &[u8]) -> Option<&str> {
    if pkt.len() < 20 || (pkt[0] & 0b1100_0000) != 0 {
        return None;
    }
    let msg_type = u16::from_be_bytes([pkt[0], pkt[1]]);
    if msg_type != STUN_BINDING_REQUEST {
        return None;
    }
    if pkt[4..8] != STUN_MAGIC_COOKIE {
        return None;
    }

    let msg_len = usize::from(u16::from_be_bytes([pkt[2], pkt[3]]));
    if !msg_len.is_multiple_of(4) || 20 + msg_len > pkt.len() {
        return None;
    }

    let attrs_end = 20 + msg_len;
    let mut offset = 20usize;
    while offset + 4 <= attrs_end {
        let attr_type = u16::from_be_bytes([pkt[offset], pkt[offset + 1]]);
        let attr_len = usize::from(u16::from_be_bytes([pkt[offset + 2], pkt[offset + 3]]));
        let val_start = offset + 4;
        let val_end = val_start + attr_len;
        if val_end > attrs_end {
            return None;
        }
        if attr_type == STUN_ATTR_USERNAME {
            let username = std::str::from_utf8(&pkt[val_start..val_end]).ok()?;
            let (local_ufrag, _) = username.split_once(':')?;
            return (!local_ufrag.is_empty()).then_some(local_ufrag);
        }
        let padded_len = (attr_len + 3) & !3;
        offset = val_start + padded_len;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::wire::AudioParams;
    use crate::transport::CaptureEndpoint;
    use crate::web::{CaptureFactory, WebBroadcastConfig, WebBroadcastManager, WebCaptureTarget};

    fn test_capture_factory() -> CaptureFactory {
        Arc::new(|| {
            #[cfg(feature = "gstreamer")]
            let endpoint = CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            };
            #[cfg(not(feature = "gstreamer"))]
            let endpoint = CaptureEndpoint::TestSineWave { freq_hz: 440.0 };

            Ok(WebCaptureTarget {
                endpoint,
                audio_params: AudioParams::default(),
                label: "test-sine-440".into(),
            })
        })
    }

    fn build_stun_binding_request(username: &str) -> Vec<u8> {
        let mut attrs = Vec::new();
        attrs.extend_from_slice(&STUN_ATTR_USERNAME.to_be_bytes());
        attrs.extend_from_slice(&(username.len() as u16).to_be_bytes());
        attrs.extend_from_slice(username.as_bytes());
        while attrs.len() % 4 != 0 {
            attrs.push(0);
        }

        let mut pkt = Vec::with_capacity(20 + attrs.len());
        pkt.extend_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
        pkt.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&STUN_MAGIC_COOKIE);
        pkt.extend_from_slice(&[0x11; 12]); // Transaction ID
        pkt.extend_from_slice(&attrs);
        pkt
    }

    #[test]
    fn stun_binding_username_demux_parses_local_ufrag() {
        let pkt = build_stun_binding_request("srvUfrag:cliUfrag");
        assert_eq!(extract_stun_binding_local_ufrag(&pkt), Some("srvUfrag"));

        // Non-STUN RTP-like packet (top 2 bits = 10) must be ignored.
        let non_stun = [0x80u8; 32];
        assert_eq!(extract_stun_binding_local_ufrag(&non_stun), None);

        // Wrong magic cookie must be ignored.
        let mut bad_magic = pkt.clone();
        bad_magic[4] = 0x00;
        assert_eq!(extract_stun_binding_local_ufrag(&bad_magic), None);

        // Missing ':' separator in USERNAME must be rejected.
        let no_colon = build_stun_binding_request("onlyonepart");
        assert_eq!(extract_stun_binding_local_ufrag(&no_colon), None);
    }

    #[test]
    fn extract_ice_ufrag_parses_sdp_lines() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ice-ufrag:abcd1234\r\n";
        assert_eq!(extract_ice_ufrag(sdp).as_deref(), Some("abcd1234"));
        assert_eq!(extract_ice_ufrag("v=0\r\ns=-\r\n"), None);
    }

    #[cfg(not(feature = "gstreamer"))]
    #[test]
    fn accept_offer_returns_service_unavailable_without_gstreamer() {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ..Default::default()
        };
        mgr.start(cfg, test_capture_factory()).expect("start");
        let rt = mgr.runtime().expect("runtime");

        let mux = RtcMuxHandle::start(RtcMuxConfig {
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            udp_port: 0,
            runtime: rt,
        })
        .expect("mux start");
        assert_ne!(mux.local_port(), 0);

        let err = mux
            .accept_offer(RtcOfferRequest {
                offer_sdp: "v=0\r\n".into(),
                remote_addr: "127.0.0.1:50000".parse().unwrap(),
                user_agent: "test".into(),
                client_name: None,
            })
            .expect_err("should fail without gstreamer");
        assert!(matches!(err, WebError::ServiceUnavailable(_)));

        mux.stop();
        mgr.stop();
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn headless_str0m_peer_negotiates_and_receives_opus_rtp_frames() {
        use str0m::media::Direction;

        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            max_clients: 2,
            idle_stop_grace: Duration::from_millis(80),
            ..Default::default()
        };
        mgr.start(cfg, test_capture_factory()).expect("start mgr");
        let rt = mgr.runtime().expect("runtime");

        let mux = RtcMuxHandle::start(RtcMuxConfig {
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            udp_port: 0,
            runtime: Arc::clone(&rt),
        })
        .expect("start mux");

        // Build a headless str0m recvonly audio peer.
        let mut client = RtcConfig::new().build(Instant::now());
        let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
        let client_addr = client_sock.local_addr().expect("client addr");
        client.add_local_candidate(Candidate::host(client_addr, "udp").unwrap());

        let mut changes = client.sdp_api();
        let _mid = changes.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, pending) = changes.apply().expect("apply offer");

        let resp = mux
            .accept_offer(RtcOfferRequest {
                offer_sdp: offer.to_sdp_string(),
                remote_addr: client_addr,
                user_agent: "str0m-headless-test/1.0".into(),
                client_name: Some("Test Phone".into()),
            })
            .expect("accept_offer succeeds");
        assert!(resp.ice_lite);
        assert!(resp.answer_sdp.contains("a=ice-lite"));
        assert_eq!(mux.client_count(), 1);
        assert_eq!(rt.active_client_count(), 1);

        let answer =
            str0m::change::SdpAnswer::from_sdp_string(&resp.answer_sdp).expect("valid SDP answer");
        client
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("accept_answer");

        let mut buf = vec![0u8; 2048];
        let mut received_opus_frames = 0usize;
        let deadline = Instant::now() + Duration::from_secs(6);

        while Instant::now() < deadline && received_opus_frames < 5 {
            let timeout = loop {
                match client.poll_output().expect("client poll_output") {
                    Output::Timeout(t) => break t,
                    Output::Transmit(tx) => {
                        let _ = client_sock.send_to(&tx.contents, tx.destination);
                    }
                    Output::Event(Event::MediaData(data)) => {
                        assert_eq!(data.params.spec().codec, Codec::Opus);
                        assert!(!data.data.is_empty());
                        received_opus_frames += 1;
                    }
                    Output::Event(_) => {}
                }
            };

            let now = Instant::now();
            if timeout <= now {
                client
                    .handle_input(Input::Timeout(now))
                    .expect("client timeout");
                continue;
            }

            let wait = (timeout - now)
                .min(Duration::from_millis(50))
                .max(Duration::from_millis(1));
            client_sock.set_read_timeout(Some(wait)).unwrap();
            buf.resize(2048, 0);

            match client_sock.recv_from(&mut buf) {
                Ok((n, source)) => {
                    if let Ok(recv) = Receive::new(Protocol::Udp, source, client_addr, &buf[..n]) {
                        client
                            .handle_input(Input::Receive(Instant::now(), recv))
                            .expect("client receive");
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    client
                        .handle_input(Input::Timeout(Instant::now()))
                        .expect("client idle timeout");
                }
                Err(e) => panic!("client recv error: {e}"),
            }
        }

        assert!(
            received_opus_frames >= 5,
            "expected >= 5 Opus RTP frames on headless str0m peer, got {received_opus_frames}"
        );

        let clients = mux.list_clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].id, resp.peer_id);
        assert_eq!(clients[0].transport, "webrtc");
        assert_eq!(clients[0].user_agent, "Test Phone");
        assert!(clients[0].bytes_sent > 0);

        assert!(mux.remove_peer(&resp.peer_id));
        assert!(!mux.remove_peer(&resp.peer_id));
        assert_eq!(mux.client_count(), 0);
        assert_eq!(rt.active_client_count(), 0);

        mux.stop();
        mgr.stop();
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn handshake_timeout_evicts_stalled_peer_and_purges_maps() {
        use str0m::media::Direction;

        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            max_clients: 2,
            idle_stop_grace: Duration::from_millis(50),
            ..Default::default()
        };
        mgr.start(cfg, test_capture_factory()).expect("start mgr");
        let rt = mgr.runtime().expect("runtime");

        let mux = RtcMuxHandle::start_with_handshake_timeout(
            RtcMuxConfig {
                bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                udp_port: 0,
                runtime: Arc::clone(&rt),
            },
            Duration::from_millis(120),
        )
        .expect("start mux");

        // Reject invalid SDP offer with BadRequest without leaking capacity or listeners.
        let bad_err = mux
            .accept_offer(RtcOfferRequest {
                offer_sdp: "not-an-sdp".into(),
                remote_addr: "127.0.0.1:50001".parse().unwrap(),
                user_agent: "bad".into(),
                client_name: None,
            })
            .expect_err("invalid SDP should fail");
        assert!(matches!(bad_err, WebError::BadRequest(_)));
        assert_eq!(rt.active_client_count(), 0);

        // Reject video-only offer with BadRequest without leaking capacity or listeners.
        let mut video_client = RtcConfig::new().build(Instant::now());
        let mut video_changes = video_client.sdp_api();
        let _vmid =
            video_changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let (video_offer, _vpending) = video_changes.apply().expect("apply video");
        let video_err = mux
            .accept_offer(RtcOfferRequest {
                offer_sdp: video_offer.to_sdp_string(),
                remote_addr: "127.0.0.1:50001".parse().unwrap(),
                user_agent: "video-only".into(),
                client_name: None,
            })
            .expect_err("video-only SDP should fail");
        assert!(matches!(video_err, WebError::BadRequest(_)));
        assert_eq!(rt.active_client_count(), 0);
        assert_eq!(rt.active_listener_count(), 0);

        // Create a valid offer, accept it, then never send any STUN/DTLS packets.
        let mut client = RtcConfig::new().build(Instant::now());
        let mut changes = client.sdp_api();
        let _mid = changes.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, _pending) = changes.apply().expect("apply");

        let resp = mux
            .accept_offer(RtcOfferRequest {
                offer_sdp: offer.to_sdp_string(),
                remote_addr: "127.0.0.1:50002".parse().unwrap(),
                user_agent: "stalled-client".into(),
                client_name: None,
            })
            .expect("accept_offer");
        let local_ufrag = extract_ice_ufrag(&resp.answer_sdp).expect("local ufrag");
        assert_eq!(mux.client_count(), 1);
        assert_eq!(rt.active_client_count(), 1);

        // Wait past the 120ms handshake timeout; mux loop should evict the stalled peer.
        let wait_until = Instant::now() + Duration::from_secs(2);
        while mux.client_count() > 0 && Instant::now() < wait_until {
            thread::sleep(Duration::from_millis(25));
        }

        assert_eq!(mux.client_count(), 0);
        assert_eq!(rt.active_client_count(), 0);
        assert!(mux.list_clients().is_empty());

        // Sending a STUN Binding Request with the evicted peer's ufrag must be ignored cleanly.
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stun_pkt = build_stun_binding_request(&format!("{local_ufrag}:remote"));
        let _ = sock.send_to(
            &stun_pkt,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), mux.local_port()),
        );
        thread::sleep(Duration::from_millis(30));
        assert_eq!(mux.client_count(), 0);

        mux.stop();
        mgr.stop();
    }
}
