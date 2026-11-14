//! Peer discovery: zero-dependency UDP broadcast (v2 protocol with RTT & optional HMAC)
//! plus pure-Rust mDNS (`mdns-sd` on `_soundhop._tcp.local.`).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

use crate::constants::{MDNS_SERVICE_TYPE, PEER_TIMEOUT_SECS, PROTOCOL_VERSION, VERSION};
use crate::security::{compute_hmac_hex, verify_hmac_hex};
use crate::util::{
    broadcast_addresses, hostname_name, is_own_non_loopback_ip, is_private_address,
    is_same_lan_subnet, local_addresses, now_secs, select_best_peer_ip, slugify,
};

pub const MAGIC: &str = "soundhop";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub version: String,
    pub protocol: u32,
    pub security: String,
    pub platform: String,
    pub last_seen: f64,
    pub source: String,
    pub rtt_ms: Option<f64>,
}

impl Peer {
    pub fn age(&self) -> f64 {
        (now_secs() - self.last_seen).max(0.0)
    }

    pub fn compatible(&self) -> bool {
        (1..=PROTOCOL_VERSION).contains(&self.protocol)
    }

    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            self.name.clone()
        } else if self.id.len() >= 8 {
            self.id[..8].to_string()
        } else {
            self.host.clone()
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "version": self.version,
            "protocol": self.protocol,
            "security": self.security,
            "platform": self.platform,
            "last_seen": self.last_seen,
            "source": self.source,
            "rtt_ms": self.rtt_ms,
            "age": (self.age() * 10.0).round() / 10.0,
            "compatible": self.compatible(),
        })
    }
}

pub struct PeerStore {
    pub timeout_secs: f64,
    peers: RwLock<HashMap<String, Peer>>,
    #[allow(clippy::type_complexity)]
    on_peer: Mutex<Option<Arc<dyn Fn(&Peer) + Send + Sync>>>,
    #[allow(clippy::type_complexity)]
    on_remove: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
}

impl Default for PeerStore {
    fn default() -> Self {
        Self::new(PEER_TIMEOUT_SECS)
    }
}

impl PeerStore {
    pub fn new(timeout_secs: f64) -> Self {
        Self {
            timeout_secs,
            peers: RwLock::new(HashMap::new()),
            on_peer: Mutex::new(None),
            on_remove: Mutex::new(None),
        }
    }

    pub fn set_on_peer<F>(&self, cb: F)
    where
        F: Fn(&Peer) + Send + Sync + 'static,
    {
        *self.on_peer.lock().unwrap() = Some(Arc::new(cb));
    }

    pub fn set_on_remove<F>(&self, cb: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        *self.on_remove.lock().unwrap() = Some(Arc::new(cb));
    }

    pub fn update(&self, mut peer: Peer) -> bool {
        if peer.id.trim().is_empty()
            || !is_private_address(&peer.host)
            || is_own_non_loopback_ip(&peer.host)
        {
            return false;
        }
        let changed = {
            let mut map = self.peers.write().unwrap();
            let prev = map.get(&peer.id);
            if let Some(p) = prev {
                if is_same_lan_subnet(&p.host) && !is_same_lan_subnet(&peer.host) {
                    peer.host = p.host.clone();
                }
            }
            let is_changed = match prev {
                None => true,
                Some(p) => {
                    p.host != peer.host
                        || p.port != peer.port
                        || p.name != peer.name
                        || p.source != peer.source
                }
            };
            if let Some(p) = prev {
                if p.last_seen > peer.last_seen {
                    peer.last_seen = p.last_seen;
                }
                if peer.rtt_ms.is_none() {
                    peer.rtt_ms = p.rtt_ms;
                }
            }
            map.insert(peer.id.clone(), peer.clone());
            is_changed
        };
        if changed {
            if let Some(cb) = self.on_peer.lock().unwrap().clone() {
                cb(&peer);
            }
        }
        changed
    }

    pub fn remove(&self, peer_id: &str) {
        let existed = self.peers.write().unwrap().remove(peer_id).is_some();
        if existed {
            if let Some(cb) = self.on_remove.lock().unwrap().clone() {
                cb(peer_id);
            }
        }
    }

    pub fn get(&self, peer_id: &str) -> Option<Peer> {
        self.peers.read().unwrap().get(peer_id).cloned()
    }

    pub fn snapshot(&self, prune: bool) -> Vec<Peer> {
        if prune {
            self.prune();
        }
        let mut list: Vec<Peer> = self.peers.read().unwrap().values().cloned().collect();
        list.sort_by(|a, b| {
            b.compatible().cmp(&a.compatible()).then_with(|| {
                a.display_name()
                    .to_lowercase()
                    .cmp(&b.display_name().to_lowercase())
            })
        });
        list
    }

    pub fn prune(&self) -> Vec<String> {
        let cutoff = now_secs() - self.timeout_secs;
        let stale: Vec<String> = {
            let mut map = self.peers.write().unwrap();
            let keys: Vec<String> = map
                .iter()
                .filter(|(_, p)| p.last_seen < cutoff)
                .map(|(k, _)| k.clone())
                .collect();
            for k in &keys {
                map.remove(k);
            }
            keys
        };
        if !stale.is_empty() {
            if let Some(cb) = self.on_remove.lock().unwrap().clone() {
                for id in &stale {
                    cb(id);
                }
            }
        }
        stale
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeaconPacket {
    pub magic: String,
    pub proto: u32,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub id: String,
    pub name: String,
    pub port: u16,
    pub ver: String,
    pub security: String,
    pub platform: String,
    #[serde(default)]
    pub ts_us: u64,
    #[serde(default)]
    pub hmac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_port: Option<u16>,
}

#[allow(clippy::too_many_arguments)]
pub fn make_beacon(
    device_id: &str,
    name: &str,
    port: u16,
    security: &str,
    msg_type: &str,
    psk_key: Option<&str>,
    ts_us: Option<u64>,
) -> Vec<u8> {
    make_beacon_with_reply_port(
        device_id, name, port, security, msg_type, psk_key, ts_us, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn make_beacon_with_reply_port(
    device_id: &str,
    name: &str,
    port: u16,
    security: &str,
    msg_type: &str,
    psk_key: Option<&str>,
    ts_us: Option<u64>,
    reply_port: Option<u16>,
) -> Vec<u8> {
    let platform = if cfg!(windows) { "windows" } else { "linux" };
    let ts = ts_us.unwrap_or_else(|| (now_secs() * 1_000_000.0) as u64);
    let mut pkt = BeaconPacket {
        magic: MAGIC.to_string(),
        proto: PROTOCOL_VERSION,
        msg_type: msg_type.to_string(),
        id: device_id.to_string(),
        name: name.to_string(),
        port,
        ver: VERSION.to_string(),
        security: security.to_string(),
        platform: platform.to_string(),
        ts_us: ts,
        hmac: String::new(),
        reply_port,
    };
    if let Some(key) = psk_key.filter(|k| !k.trim().is_empty()) {
        let sig_input = format!(
            "{}:{}:{}:{}:{}:{}:{}",
            pkt.magic, pkt.proto, pkt.msg_type, pkt.id, pkt.port, pkt.security, pkt.ts_us
        );
        pkt.hmac = compute_hmac_hex(key, sig_input.as_bytes());
    }
    serde_json::to_vec(&pkt).unwrap_or_default()
}

pub fn parse_beacon(
    data: &[u8],
    host: &str,
    source: &str,
    psk_key: Option<&str>,
) -> (Option<BeaconPacket>, Option<Peer>) {
    if !is_private_address(host) || is_own_non_loopback_ip(host) {
        return (None, None);
    }
    let Ok(pkt) = serde_json::from_slice::<BeaconPacket>(data) else {
        return (None, None);
    };
    if pkt.magic != MAGIC {
        return (None, None);
    }
    if let Some(key) = psk_key.filter(|k| !k.trim().is_empty()) {
        if pkt.hmac.is_empty() {
            return (None, None);
        }
        let sig_input = format!(
            "{}:{}:{}:{}:{}:{}:{}",
            pkt.magic, pkt.proto, pkt.msg_type, pkt.id, pkt.port, pkt.security, pkt.ts_us
        );
        if !verify_hmac_hex(key, sig_input.as_bytes(), &pkt.hmac) {
            return (None, None);
        }
    }
    if pkt.msg_type != "offer" {
        return (Some(pkt), None);
    }
    let raw_id = pkt.id.trim().to_string();
    if raw_id.is_empty() || raw_id.len() > 128 || pkt.port == 0 {
        return (Some(pkt), None);
    }
    let name = if pkt.name.trim().is_empty() {
        hostname_name()
    } else {
        pkt.name.chars().take(100).collect()
    };
    let now_us = (now_secs() * 1_000_000.0) as u64;
    let rtt_ms = if pkt.ts_us > 0 && now_us >= pkt.ts_us {
        let diff_ms = (now_us - pkt.ts_us) as f64 / 1000.0;
        if diff_ms < 5000.0 {
            Some((diff_ms * 100.0).round() / 100.0)
        } else {
            None
        }
    } else {
        None
    };

    let peer = Peer {
        id: raw_id,
        name,
        host: host.to_string(),
        port: pkt.port,
        version: pkt.ver.clone(),
        protocol: pkt.proto,
        security: pkt.security.clone(),
        platform: pkt.platform.clone(),
        last_seen: now_secs(),
        source: source.to_string(),
        rtt_ms,
    };
    (Some(pkt), Some(peer))
}

fn bind_udp_reusable(port: u16) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = sock.set_reuse_address(true);
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let optval: libc::c_int = 1;
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of_val(&optval) as libc::socklen_t,
            );
        }
    }
    let _ = sock.set_broadcast(true);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    sock.bind(&addr.into())?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(sock.into())
}

pub struct DiscoveryManager {
    pub device_id: String,
    pub name: String,
    pub audio_port: u16,
    pub discovery_port: u16,
    pub security: String,
    pub psk_key: Option<String>,
    pub interval_secs: f64,
    pub use_udp: bool,
    pub use_mdns: bool,
    pub store: Arc<PeerStore>,
    advertising: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    udp_socket: Mutex<Option<UdpSocket>>,
    udp_thread: Mutex<Option<JoinHandle<()>>>,
    mdns_daemon: Mutex<Option<ServiceDaemon>>,
    mdns_thread: Mutex<Option<JoinHandle<()>>>,
}

impl DiscoveryManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_id: String,
        name: String,
        audio_port: u16,
        discovery_port: u16,
        security: String,
        psk_key: Option<String>,
        interval_secs: f64,
        use_udp: bool,
        use_mdns: bool,
    ) -> Self {
        Self {
            device_id,
            name,
            audio_port,
            discovery_port,
            security,
            psk_key,
            interval_secs: interval_secs.max(1.0),
            use_udp,
            use_mdns,
            store: Arc::new(PeerStore::new(PEER_TIMEOUT_SECS)),
            advertising: Arc::new(AtomicBool::new(false)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            udp_socket: Mutex::new(None),
            udp_thread: Mutex::new(None),
            mdns_daemon: Mutex::new(None),
            mdns_thread: Mutex::new(None),
        }
    }

    pub fn start(&self, advertising: bool) -> Vec<String> {
        let mut warnings = Vec::new();
        self.advertising.store(advertising, Ordering::SeqCst);
        self.stop_flag.store(false, Ordering::SeqCst);

        if self.use_udp {
            let bind_res = bind_udp_reusable(self.discovery_port).or_else(|_| bind_udp_reusable(0));
            match bind_res {
                Ok(sock) => {
                    let local_port = sock.local_addr().map(|a| a.port()).ok();
                    let sock_clone = sock.try_clone().ok();
                    *self.udp_socket.lock().unwrap() = sock_clone;
                    let stop = Arc::clone(&self.stop_flag);
                    let adv = Arc::clone(&self.advertising);
                    let store = Arc::clone(&self.store);
                    let dev_id = self.device_id.clone();
                    let dev_name = self.name.clone();
                    let audio_port = self.audio_port;
                    let disc_port = self.discovery_port;
                    let sec = self.security.clone();
                    let psk = self.psk_key.clone();
                    let interval = Duration::from_secs_f64(self.interval_secs);

                    let handle = thread::spawn(move || {
                        let mut next_beacon = Instant::now();
                        let mut buf = [0u8; 4096];
                        // Send initial probe
                        let probe = make_beacon_with_reply_port(
                            &dev_id,
                            &dev_name,
                            audio_port,
                            &sec,
                            "probe",
                            psk.as_deref(),
                            None,
                            local_port,
                        );
                        for target in broadcast_addresses(true) {
                            if let Ok(ip) = target.parse::<IpAddr>() {
                                let _ = sock.send_to(&probe, SocketAddr::new(ip, disc_port));
                            }
                        }

                        while !stop.load(Ordering::SeqCst) {
                            if Instant::now() >= next_beacon {
                                if adv.load(Ordering::SeqCst) {
                                    let offer = make_beacon(
                                        &dev_id,
                                        &dev_name,
                                        audio_port,
                                        &sec,
                                        "offer",
                                        psk.as_deref(),
                                        None,
                                    );
                                    for target in broadcast_addresses(true) {
                                        if let Ok(ip) = target.parse::<IpAddr>() {
                                            let _ = sock
                                                .send_to(&offer, SocketAddr::new(ip, disc_port));
                                        }
                                    }
                                }
                                // Also send periodic probe so peers & RTT stay fresh even if
                                // another local process shares port 47777 or we are on an ephemeral port
                                let probe_pkt = make_beacon_with_reply_port(
                                    &dev_id,
                                    &dev_name,
                                    audio_port,
                                    &sec,
                                    "probe",
                                    psk.as_deref(),
                                    None,
                                    local_port,
                                );
                                for target in broadcast_addresses(true) {
                                    if let Ok(ip) = target.parse::<IpAddr>() {
                                        let _ = sock
                                            .send_to(&probe_pkt, SocketAddr::new(ip, disc_port));
                                    }
                                }
                                next_beacon = Instant::now() + interval;
                            }

                            if let Ok((len, sender)) = sock.recv_from(&mut buf) {
                                let host = sender.ip().to_string();
                                let (pkt_opt, peer_opt) =
                                    parse_beacon(&buf[..len], &host, "udp", psk.as_deref());
                                if let Some(pkt) = pkt_opt {
                                    if pkt.id == dev_id {
                                        continue;
                                    }
                                    if pkt.msg_type == "probe" && adv.load(Ordering::SeqCst) {
                                        let reply = make_beacon(
                                            &dev_id,
                                            &dev_name,
                                            audio_port,
                                            &sec,
                                            "offer",
                                            psk.as_deref(),
                                            Some(pkt.ts_us),
                                        );
                                        let _ = sock.send_to(&reply, sender);
                                        if let Some(rp) = pkt.reply_port.filter(|&p| p > 0) {
                                            if rp != sender.port() {
                                                let _ = sock.send_to(
                                                    &reply,
                                                    SocketAddr::new(sender.ip(), rp),
                                                );
                                            }
                                        }
                                    }
                                }
                                if let Some(peer) = peer_opt {
                                    if peer.id != dev_id {
                                        store.update(peer);
                                    }
                                }
                            }
                        }
                    });
                    *self.udp_thread.lock().unwrap() = Some(handle);
                }
                Err(err) => {
                    warnings.push(format!(
                        "UDP discovery unavailable on port {}: {err}",
                        self.discovery_port
                    ));
                }
            }
        }

        if self.use_mdns {
            match ServiceDaemon::new() {
                Ok(mdns) => {
                    if advertising {
                        let instance_name = format!(
                            "{}-{}",
                            slugify(&self.name, 20),
                            &self.device_id[..self.device_id.len().min(6)]
                        );
                        let host_fqdn = format!("{instance_name}.local.");
                        let addrs = local_addresses(false);
                        let ip_str = addrs.first().map(|s| s.as_str()).unwrap_or("");
                        let props = [
                            ("id", self.device_id.as_str()),
                            ("name", self.name.as_str()),
                            ("ver", VERSION),
                            ("proto", "2"),
                            ("security", self.security.as_str()),
                        ];
                        if let Ok(info) = ServiceInfo::new(
                            MDNS_SERVICE_TYPE,
                            &instance_name,
                            &host_fqdn,
                            ip_str,
                            self.audio_port,
                            &props[..],
                        ) {
                            let _ = mdns.register(info);
                        }
                    }
                    if let Ok(receiver) = mdns.browse(MDNS_SERVICE_TYPE) {
                        let stop = Arc::clone(&self.stop_flag);
                        let store = Arc::clone(&self.store);
                        let own_id = self.device_id.clone();
                        let handle = thread::spawn(move || {
                            while !stop.load(Ordering::SeqCst) {
                                if let Ok(ServiceEvent::ServiceResolved(info)) =
                                    receiver.recv_timeout(Duration::from_millis(250))
                                {
                                    let id =
                                        info.get_property_val_str("id").unwrap_or("").to_string();
                                    if id.is_empty() || id == own_id {
                                        continue;
                                    }
                                    let name = info
                                        .get_property_val_str("name")
                                        .unwrap_or("SoundHop Peer")
                                        .to_string();
                                    let ver = info
                                        .get_property_val_str("ver")
                                        .unwrap_or(VERSION)
                                        .to_string();
                                    let sec = info
                                        .get_property_val_str("security")
                                        .unwrap_or("trusted-lan")
                                        .to_string();
                                    let proto = info
                                        .get_property_val_str("proto")
                                        .and_then(|p| p.parse::<u32>().ok())
                                        .unwrap_or(PROTOCOL_VERSION);
                                    if let Some(host) = select_best_peer_ip(info.get_addresses()) {
                                        if is_private_address(&host) {
                                            store.update(Peer {
                                                id,
                                                name,
                                                host,
                                                port: info.get_port(),
                                                version: ver,
                                                protocol: proto,
                                                security: sec,
                                                platform: "linux".to_string(),
                                                last_seen: now_secs(),
                                                source: "mdns".to_string(),
                                                rtt_ms: None,
                                            });
                                        }
                                    }
                                }
                            }
                        });
                        *self.mdns_thread.lock().unwrap() = Some(handle);
                    }
                    *self.mdns_daemon.lock().unwrap() = Some(mdns);
                }
                Err(err) => {
                    warnings.push(format!("mDNS discovery unavailable: {err}"));
                }
            }
        }

        warnings
    }

    pub fn set_advertising(&self, enabled: bool) {
        self.advertising.store(enabled, Ordering::SeqCst);
        if enabled {
            self.probe();
        }
    }

    pub fn probe(&self) {
        if let Some(sock) = self.udp_socket.lock().unwrap().as_ref() {
            let local_port = sock.local_addr().map(|a| a.port()).ok();
            let probe = make_beacon_with_reply_port(
                &self.device_id,
                &self.name,
                self.audio_port,
                &self.security,
                "probe",
                self.psk_key.as_deref(),
                None,
                local_port,
            );
            for target in broadcast_addresses(true) {
                if let Ok(ip) = target.parse::<IpAddr>() {
                    let _ = sock.send_to(&probe, SocketAddr::new(ip, self.discovery_port));
                }
            }
        }
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.store.snapshot(true)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        *self.udp_socket.lock().unwrap() = None;
        if let Some(h) = self.udp_thread.lock().unwrap().take() {
            let _ = h.join();
        }
        if let Some(mdns) = self.mdns_daemon.lock().unwrap().take() {
            let _ = mdns.shutdown();
        }
        if let Some(h) = self.mdns_thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

/// Perform a one-shot discovery scan when the daemon is not running.
pub fn discover_once(timeout_secs: f64, discovery_port: u16, own_id: &str) -> Vec<Peer> {
    let store = PeerStore::new(PEER_TIMEOUT_SECS);
    let Ok(sock) = bind_udp_reusable(0) else {
        return Vec::new();
    };
    let local_port = sock.local_addr().map(|a| a.port()).ok();
    let probe = make_beacon_with_reply_port(
        own_id,
        &hostname_name(),
        0,
        "trusted-lan",
        "probe",
        None,
        None,
        local_port,
    );
    for target in broadcast_addresses(true) {
        if let Ok(ip) = target.parse::<IpAddr>() {
            let _ = sock.send_to(&probe, SocketAddr::new(ip, discovery_port));
        }
    }
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.2));
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        if let Ok((len, sender)) = sock.recv_from(&mut buf) {
            let host = sender.ip().to_string();
            let (_, peer_opt) = parse_beacon(&buf[..len], &host, "udp", None);
            if let Some(peer) = peer_opt {
                if peer.id != own_id {
                    store.update(peer);
                }
            }
        }
    }
    store.snapshot(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_beacon_encode_decode_with_hmac_and_rtt() {
        let key = "0123456789abcdef";
        let raw = make_beacon(
            "dev_abc_123",
            "Studio Rig",
            4713,
            "psk",
            "offer",
            Some(key),
            None,
        );
        let (pkt, peer) = parse_beacon(&raw, "192.168.1.42", "udp", Some(key));
        assert!(pkt.is_some());
        let peer = peer.unwrap();
        assert_eq!(peer.id, "dev_abc_123");
        assert_eq!(peer.name, "Studio Rig");
        assert_eq!(peer.port, 4713);
        assert_eq!(peer.security, "psk");
        assert!(peer.rtt_ms.is_some());

        // Wrong key should reject
        let (_, bad_peer) = parse_beacon(&raw, "192.168.1.42", "udp", Some("wrong_key"));
        assert!(bad_peer.is_none());
    }

    #[test]
    fn test_parse_beacon_rejects_empty_hmac_when_psk_set() {
        let unsigned = make_beacon("dev_unsigned", "Spoof", 4713, "psk", "offer", None, None);
        let (pkt, peer) = parse_beacon(&unsigned, "192.168.1.50", "udp", Some("secret-key"));
        assert!(pkt.is_none());
        assert!(peer.is_none());
    }
}
