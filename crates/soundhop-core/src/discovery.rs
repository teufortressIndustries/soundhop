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
    is_same_lan_subnet, local_addresses, lock_or_recover, now_secs, select_best_peer_ip, slugify,
};

pub const MAGIC: &str = "soundhop";

/// Maximum allowed clock skew / replay window (30 seconds) for PSK-signed UDP beacons.
pub const MAX_BEACON_SKEW_US: u64 = 30_000_000;

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
    v3_meta: RwLock<HashMap<String, BeaconV3Meta>>,
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
            v3_meta: RwLock::new(HashMap::new()),
            on_peer: Mutex::new(None),
            on_remove: Mutex::new(None),
        }
    }

    pub fn update_v3_meta(&self, peer_id: &str, meta: BeaconV3Meta) {
        if peer_id.trim().is_empty() || meta.is_empty() {
            return;
        }
        let mut guard = self.v3_meta.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(peer_id.to_string(), meta);
    }

    pub fn v3_meta(&self, peer_id: &str) -> Option<BeaconV3Meta> {
        self.v3_meta
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(peer_id)
            .cloned()
    }

    pub fn set_on_peer<F>(&self, cb: F)
    where
        F: Fn(&Peer) + Send + Sync + 'static,
    {
        *lock_or_recover(&self.on_peer) = Some(Arc::new(cb));
    }

    pub fn set_on_remove<F>(&self, cb: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        *lock_or_recover(&self.on_remove) = Some(Arc::new(cb));
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
                if matches!(p.security.as_str(), "psk" | "pin" | "paired") && peer.source == "mdns"
                {
                    return false;
                }
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
            if let Some(cb) = lock_or_recover(&self.on_peer).clone() {
                cb(&peer);
            }
        }
        changed
    }

    pub fn remove(&self, peer_id: &str) {
        let existed = self.peers.write().unwrap().remove(peer_id).is_some();
        self.v3_meta
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id);
        if existed {
            if let Some(cb) = lock_or_recover(&self.on_remove).clone() {
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
            {
                let mut v3 = self.v3_meta.write().unwrap_or_else(|e| e.into_inner());
                for id in &stale {
                    v3.remove(id);
                }
            }
            if let Some(cb) = lock_or_recover(&self.on_remove).clone() {
                for id in &stale {
                    cb(id);
                }
            }
        }
        stale
    }
}

/// Optional v3 / Pairing v2 metadata advertised in UDP beacons and mDNS TXT records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BeaconV3Meta {
    pub v3_port: Option<u16>,
    pub pair_port: Option<u16>,
    pub pub_fp: Option<String>,
    pub caps: Vec<String>,
}

impl BeaconV3Meta {
    /// Returns `true` when the peer advertises a valid SHT3 TCP port and the `"sht3"` capability.
    pub fn supports_sht3(&self) -> bool {
        self.v3_port.is_some_and(|p| p > 0)
            && self.caps.iter().any(|c| c.eq_ignore_ascii_case("sht3"))
    }

    pub fn is_empty(&self) -> bool {
        self.v3_port.is_none()
            && self.pair_port.is_none()
            && self.pub_fp.is_none()
            && self.caps.is_empty()
    }
}

/// Default Transport v3 capabilities advertised by this build.
pub fn default_v3_caps() -> Vec<String> {
    #[cfg(feature = "opus")]
    {
        vec!["sht3".to_string(), "opus".to_string(), "pcm".to_string()]
    }
    #[cfg(not(feature = "opus"))]
    {
        vec!["sht3".to_string(), "pcm".to_string()]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v3_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pub_fp: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
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

fn beacon_signing_input(pkt: &BeaconPacket) -> String {
    let base = format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        pkt.magic,
        pkt.proto,
        pkt.msg_type,
        pkt.id,
        pkt.name,
        pkt.port,
        pkt.ver,
        pkt.security,
        pkt.platform,
        pkt.ts_us,
        pkt.reply_port.unwrap_or(0)
    );
    if pkt.v3_port.is_none()
        && pkt.pair_port.is_none()
        && pkt.pub_fp.is_none()
        && pkt.caps.is_empty()
    {
        base
    } else {
        format!(
            "{base}:{}:{}:{}:{}",
            pkt.v3_port.unwrap_or(0),
            pkt.pair_port.unwrap_or(0),
            pkt.pub_fp.as_deref().unwrap_or(""),
            pkt.caps.join(",")
        )
    }
}

/// Returns the normalized platform identifier for this host (`"windows"`, `"linux"`, `"macos"`, ...).
pub fn current_platform() -> &'static str {
    std::env::consts::OS
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
    make_beacon_v3(
        device_id,
        name,
        port,
        security,
        msg_type,
        psk_key,
        ts_us,
        reply_port,
        &BeaconV3Meta::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn make_beacon_v3(
    device_id: &str,
    name: &str,
    port: u16,
    security: &str,
    msg_type: &str,
    psk_key: Option<&str>,
    ts_us: Option<u64>,
    reply_port: Option<u16>,
    v3: &BeaconV3Meta,
) -> Vec<u8> {
    let platform = current_platform();
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
        v3_port: v3.v3_port,
        pair_port: v3.pair_port,
        pub_fp: v3.pub_fp.clone(),
        caps: v3.caps.clone(),
    };
    if let Some(key) = psk_key.filter(|k| !k.trim().is_empty()) {
        let sig_input = beacon_signing_input(&pkt);
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
        if pkt.hmac.is_empty() || pkt.ts_us == 0 {
            return (None, None);
        }
        let now_us = (now_secs() * 1_000_000.0) as u64;
        if now_us.abs_diff(pkt.ts_us) > MAX_BEACON_SKEW_US {
            return (None, None);
        }
        let sig_input = beacon_signing_input(&pkt);
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

#[cfg(windows)]
fn suppress_udp_connreset(sock: &Socket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SIO_UDP_CONNRESET, SOCKET};

    // WSAIoctl is gated behind Win32_System_IO in windows-sys, so link ws2_32 directly.
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAIoctl(
            s: SOCKET,
            dw_io_control_code: u32,
            lpv_in_buffer: *const core::ffi::c_void,
            cb_in_buffer: u32,
            lpv_out_buffer: *mut core::ffi::c_void,
            cb_out_buffer: u32,
            lpcb_bytes_returned: *mut u32,
            lp_overlapped: *mut core::ffi::c_void,
            lp_completion_routine: *const core::ffi::c_void,
        ) -> i32;
    }

    let enable: u32 = 0; // FALSE: suppress WSAECONNRESET (10054) on ICMP Port Unreachable
    let mut bytes_returned: u32 = 0;
    unsafe {
        let _ = WSAIoctl(
            sock.as_raw_socket() as SOCKET,
            SIO_UDP_CONNRESET,
            (&enable as *const u32).cast(),
            std::mem::size_of_val(&enable) as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
    }
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
    #[cfg(windows)]
    suppress_udp_connreset(&sock);
    let _ = sock.set_broadcast(true);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    sock.bind(&addr.into())?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(sock.into())
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_mdns_txt_properties<'a>(
    device_id: &'a str,
    name: &'a str,
    security: &'a str,
    platform: &'a str,
) -> [(&'static str, &'a str); 6] {
    [
        ("id", device_id),
        ("name", name),
        ("ver", VERSION),
        ("proto", "2"),
        ("security", security),
        ("platform", platform),
    ]
}

fn build_mdns_txt_properties_v3(
    device_id: &str,
    name: &str,
    security: &str,
    platform: &str,
    v3: &BeaconV3Meta,
) -> Vec<(&'static str, String)> {
    let mut props = vec![
        ("id", device_id.to_string()),
        ("name", name.to_string()),
        ("ver", VERSION.to_string()),
        ("proto", "2".to_string()),
        ("security", security.to_string()),
        ("platform", platform.to_string()),
    ];
    if let Some(v3_port) = v3.v3_port.filter(|&p| p > 0) {
        props.push(("v3_port", v3_port.to_string()));
    }
    if let Some(pair_port) = v3.pair_port.filter(|&p| p > 0) {
        props.push(("pair_port", pair_port.to_string()));
    }
    if let Some(pub_fp) = v3
        .pub_fp
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        props.push(("pub_fp", pub_fp.to_string()));
    }
    if !v3.caps.is_empty() {
        props.push(("caps", v3.caps.join(",")));
    }
    props
}

/// Extract optional v3 / Pairing v2 TXT metadata from a resolved mDNS [`ServiceInfo`].
pub fn parse_mdns_v3_metadata(info: &ServiceInfo) -> BeaconV3Meta {
    let v3_port = info
        .get_property_val_str("v3_port")
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|&p| p > 0);
    let pair_port = info
        .get_property_val_str("pair_port")
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|&p| p > 0);
    let pub_fp = info
        .get_property_val_str("pub_fp")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let caps = info
        .get_property_val_str("caps")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    BeaconV3Meta {
        v3_port,
        pair_port,
        pub_fp,
        caps,
    }
}

fn parse_mdns_service_info(info: &ServiceInfo, own_id: &str) -> Option<Peer> {
    let id = info.get_property_val_str("id").unwrap_or("").to_string();
    if id.is_empty() || id == own_id {
        return None;
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
    let platform = info
        .get_property_val_str("platform")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let host = select_best_peer_ip(info.get_addresses())?;
    if !is_private_address(&host) {
        return None;
    }
    Some(Peer {
        id,
        name,
        host,
        port: info.get_port(),
        version: ver,
        protocol: proto,
        security: sec,
        platform,
        last_seen: now_secs(),
        source: "mdns".to_string(),
        rtt_ms: None,
    })
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
    pub v3_meta: BeaconV3Meta,
    pub store: Arc<PeerStore>,
    advertising: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    udp_socket: Mutex<Option<UdpSocket>>,
    udp_thread: Mutex<Option<JoinHandle<()>>>,
    mdns_daemon: Mutex<Option<ServiceDaemon>>,
    mdns_thread: Mutex<Option<JoinHandle<()>>>,
    mdns_fullname: Mutex<Option<String>>,
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
            v3_meta: BeaconV3Meta::default(),
            store: Arc::new(PeerStore::new(PEER_TIMEOUT_SECS)),
            advertising: Arc::new(AtomicBool::new(false)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            udp_socket: Mutex::new(None),
            udp_thread: Mutex::new(None),
            mdns_daemon: Mutex::new(None),
            mdns_thread: Mutex::new(None),
            mdns_fullname: Mutex::new(None),
        }
    }

    fn build_mdns_service_info(&self) -> Option<ServiceInfo> {
        let id_prefix: String = self.device_id.chars().take(6).collect();
        let instance_name = format!("{}-{}", slugify(&self.name, 20), id_prefix);
        let host_fqdn = format!("{instance_name}.local.");
        let addrs = local_addresses(false);
        let ip_str = addrs.first().map(|s| s.as_str()).unwrap_or("");
        let props_owned = build_mdns_txt_properties_v3(
            &self.device_id,
            &self.name,
            &self.security,
            current_platform(),
            &self.v3_meta,
        );
        let props_refs: Vec<(&str, &str)> =
            props_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &instance_name,
            &host_fqdn,
            ip_str,
            self.audio_port,
            &props_refs[..],
        )
        .ok()
    }

    pub fn with_v3_meta(mut self, v3_meta: BeaconV3Meta) -> Self {
        self.v3_meta = v3_meta;
        self
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
                    *lock_or_recover(&self.udp_socket) = sock_clone;
                    let stop = Arc::clone(&self.stop_flag);
                    let adv = Arc::clone(&self.advertising);
                    let store = Arc::clone(&self.store);
                    let dev_id = self.device_id.clone();
                    let dev_name = self.name.clone();
                    let audio_port = self.audio_port;
                    let disc_port = self.discovery_port;
                    let sec = self.security.clone();
                    let psk = self.psk_key.clone();
                    let v3_meta = self.v3_meta.clone();
                    let interval = Duration::from_secs_f64(self.interval_secs);

                    let handle = thread::spawn(move || {
                        let mut next_beacon = Instant::now();
                        let mut buf = [0u8; 4096];
                        // Send initial probe
                        let probe = make_beacon_v3(
                            &dev_id,
                            &dev_name,
                            audio_port,
                            &sec,
                            "probe",
                            psk.as_deref(),
                            None,
                            local_port,
                            &v3_meta,
                        );
                        for target in broadcast_addresses(true) {
                            if let Ok(ip) = target.parse::<IpAddr>() {
                                let _ = sock.send_to(&probe, SocketAddr::new(ip, disc_port));
                            }
                        }

                        while !stop.load(Ordering::SeqCst) {
                            if Instant::now() >= next_beacon {
                                if adv.load(Ordering::SeqCst) {
                                    let offer = make_beacon_v3(
                                        &dev_id,
                                        &dev_name,
                                        audio_port,
                                        &sec,
                                        "offer",
                                        psk.as_deref(),
                                        None,
                                        None,
                                        &v3_meta,
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
                                let probe_pkt = make_beacon_v3(
                                    &dev_id,
                                    &dev_name,
                                    audio_port,
                                    &sec,
                                    "probe",
                                    psk.as_deref(),
                                    None,
                                    local_port,
                                    &v3_meta,
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
                                        let reply = make_beacon_v3(
                                            &dev_id,
                                            &dev_name,
                                            audio_port,
                                            &sec,
                                            "offer",
                                            psk.as_deref(),
                                            Some(pkt.ts_us),
                                            None,
                                            &v3_meta,
                                        );
                                        let reply_target = pkt
                                            .reply_port
                                            .filter(|&p| p > 0)
                                            .map(|rp| SocketAddr::new(sender.ip(), rp))
                                            .unwrap_or(sender);
                                        let _ = sock.send_to(&reply, reply_target);
                                    }
                                    if let Some(peer) = peer_opt {
                                        if peer.id != dev_id {
                                            let meta = BeaconV3Meta {
                                                v3_port: pkt.v3_port,
                                                pair_port: pkt.pair_port,
                                                pub_fp: pkt.pub_fp,
                                                caps: pkt.caps,
                                            };
                                            store.update_v3_meta(&peer.id, meta);
                                            store.update(peer);
                                        }
                                    }
                                }
                            }
                        }
                    });
                    *lock_or_recover(&self.udp_thread) = Some(handle);
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
                        if let Some(info) = self.build_mdns_service_info() {
                            *lock_or_recover(&self.mdns_fullname) =
                                Some(info.get_fullname().to_string());
                            let _ = mdns.register(info);
                        }
                    }
                    let psk_mode = self.psk_key.as_ref().is_some_and(|k| !k.trim().is_empty())
                        || matches!(self.security.as_str(), "psk" | "pin" | "paired");
                    if !psk_mode {
                        if let Ok(receiver) = mdns.browse(MDNS_SERVICE_TYPE) {
                            let stop = Arc::clone(&self.stop_flag);
                            let store = Arc::clone(&self.store);
                            let own_id = self.device_id.clone();
                            let handle = thread::spawn(move || {
                                while !stop.load(Ordering::SeqCst) {
                                    if let Ok(ServiceEvent::ServiceResolved(info)) =
                                        receiver.recv_timeout(Duration::from_millis(250))
                                    {
                                        if let Some(peer) = parse_mdns_service_info(&info, &own_id)
                                        {
                                            let meta = parse_mdns_v3_metadata(&info);
                                            store.update_v3_meta(&peer.id, meta);
                                            store.update(peer);
                                        }
                                    }
                                }
                            });
                            *lock_or_recover(&self.mdns_thread) = Some(handle);
                        }
                    }
                    *lock_or_recover(&self.mdns_daemon) = Some(mdns);
                }
                Err(err) => {
                    warnings.push(format!("mDNS discovery unavailable: {err}"));
                }
            }
        }

        warnings
    }

    pub fn set_advertising(&self, enabled: bool) {
        self.advertising.store(enabled, Ordering::Relaxed);
        {
            let mdns_guard = lock_or_recover(&self.mdns_daemon);
            if let Some(ref mdns) = *mdns_guard {
                if !enabled {
                    if let Some(fullname) = lock_or_recover(&self.mdns_fullname).take() {
                        let _ = mdns.unregister(&fullname);
                    }
                } else {
                    let mut fullname_guard = lock_or_recover(&self.mdns_fullname);
                    if fullname_guard.is_none() {
                        if let Some(info) = self.build_mdns_service_info() {
                            *fullname_guard = Some(info.get_fullname().to_string());
                            let _ = mdns.register(info);
                        }
                    }
                }
            }
        }
        if enabled {
            self.probe();
        }
    }

    pub fn probe(&self) {
        if let Some(sock) = lock_or_recover(&self.udp_socket).as_ref() {
            let local_port = sock.local_addr().map(|a| a.port()).ok();
            let probe = make_beacon_v3(
                &self.device_id,
                &self.name,
                self.audio_port,
                &self.security,
                "probe",
                self.psk_key.as_deref(),
                None,
                local_port,
                &self.v3_meta,
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

    pub fn peer_v3_meta(&self, peer_id: &str) -> Option<BeaconV3Meta> {
        self.store.v3_meta(peer_id)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        *lock_or_recover(&self.udp_socket) = None;
        if let Some(h) = lock_or_recover(&self.udp_thread).take() {
            let _ = h.join();
        }
        *lock_or_recover(&self.mdns_fullname) = None;
        if let Some(mdns) = lock_or_recover(&self.mdns_daemon).take() {
            let _ = mdns.shutdown();
        }
        if let Some(h) = lock_or_recover(&self.mdns_thread).take() {
            let _ = h.join();
        }
    }
}

/// Perform a one-shot discovery scan when the daemon is not running.
pub fn discover_once(timeout_secs: f64, discovery_port: u16, own_id: &str) -> Vec<Peer> {
    discover_once_with_psk(timeout_secs, discovery_port, own_id, None)
}

/// Perform a one-shot discovery scan, enforcing HMAC verification when `psk_key` is set.
pub fn discover_once_with_psk(
    timeout_secs: f64,
    discovery_port: u16,
    own_id: &str,
    psk_key: Option<&str>,
) -> Vec<Peer> {
    let store = PeerStore::new(PEER_TIMEOUT_SECS);
    let Ok(sock) = bind_udp_reusable(0) else {
        return Vec::new();
    };
    let local_port = sock.local_addr().map(|a| a.port()).ok();
    let psk_clean = psk_key.filter(|k| !k.trim().is_empty());
    let security = if psk_clean.is_some() {
        "psk"
    } else {
        "trusted-lan"
    };
    let probe = make_beacon_with_reply_port(
        own_id,
        &hostname_name(),
        0,
        security,
        "probe",
        psk_clean,
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
            let (_, peer_opt) = parse_beacon(&buf[..len], &host, "udp", psk_clean);
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

    #[test]
    fn test_parse_beacon_rejects_tampered_metadata_and_stale_replay() {
        let key = "0123456789abcdef0123456789abcdef";
        let raw = make_beacon_with_reply_port(
            "dev_abc",
            "Original Name",
            4713,
            "psk",
            "offer",
            Some(key),
            None,
            Some(47777),
        );
        let mut parsed: BeaconPacket = serde_json::from_slice(&raw).unwrap();
        parsed.name = "Spoofed Name".to_string();
        let tampered_name = serde_json::to_vec(&parsed).unwrap();
        assert!(
            parse_beacon(&tampered_name, "192.168.1.50", "udp", Some(key))
                .1
                .is_none()
        );

        let mut parsed_rp: BeaconPacket = serde_json::from_slice(&raw).unwrap();
        parsed_rp.reply_port = Some(9999);
        let tampered_rp = serde_json::to_vec(&parsed_rp).unwrap();
        assert!(parse_beacon(&tampered_rp, "192.168.1.50", "udp", Some(key))
            .0
            .is_none());

        let stale_ts = ((now_secs() - 60.0).max(1.0) * 1_000_000.0) as u64;
        let stale = make_beacon(
            "dev_abc",
            "Original Name",
            4713,
            "psk",
            "offer",
            Some(key),
            Some(stale_ts),
        );
        assert!(parse_beacon(&stale, "192.168.1.50", "udp", Some(key))
            .1
            .is_none());
    }

    #[test]
    fn test_peer_store_blocks_mdns_overwrite_of_psk_peer() {
        let store = PeerStore::new(20.0);
        let psk_peer = Peer {
            id: "peer-1".to_string(),
            name: "Studio".to_string(),
            host: "192.168.1.20".to_string(),
            port: 4713,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "psk".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "udp".to_string(),
            rtt_ms: None,
        };
        assert!(store.update(psk_peer));

        let spoof_mdns = Peer {
            id: "peer-1".to_string(),
            name: "Studio".to_string(),
            host: "192.168.1.99".to_string(),
            port: 9999,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "mdns".to_string(),
            rtt_ms: None,
        };
        assert!(!store.update(spoof_mdns));
        let stored = store.get("peer-1").unwrap();
        assert_eq!(stored.host, "192.168.1.20");
        assert_eq!(stored.port, 4713);
    }

    #[test]
    fn test_mdns_platform_property_propagation() {
        for platform in ["windows", "linux", "macos"] {
            let props = build_mdns_txt_properties("peer-42", "Desk Rig", "trusted-lan", platform);
            assert!(props.contains(&("platform", platform)));

            let info = ServiceInfo::new(
                MDNS_SERVICE_TYPE,
                "desk-rig-peer42",
                "desk-rig-peer42.local.",
                "10.254.254.42",
                4713,
                &props[..],
            )
            .unwrap();

            let peer = parse_mdns_service_info(&info, "own-device-id").unwrap();
            assert_eq!(peer.id, "peer-42");
            assert_eq!(peer.name, "Desk Rig");
            assert_eq!(peer.port, 4713);
            assert_eq!(peer.platform, platform);
            assert_eq!(peer.source, "mdns");
        }

        // Legacy mDNS record without platform property falls back cleanly
        let legacy_props = [
            ("id", "peer-legacy"),
            ("name", "Legacy Box"),
            ("ver", VERSION),
            ("proto", "2"),
            ("security", "trusted-lan"),
        ];
        let legacy_info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            "legacy-box",
            "legacy-box.local.",
            "10.254.254.43",
            4713,
            &legacy_props[..],
        )
        .unwrap();
        let legacy_peer = parse_mdns_service_info(&legacy_info, "own-device-id").unwrap();
        assert_eq!(legacy_peer.platform, "unknown");

        // Own device ID is filtered out
        assert!(parse_mdns_service_info(&legacy_info, "peer-legacy").is_none());
    }

    #[test]
    fn test_beacon_and_mdns_use_current_platform() {
        let raw = make_beacon("dev-os", "Host", 4713, "trusted-lan", "offer", None, None);
        let (pkt, peer) = parse_beacon(&raw, "10.254.254.44", "udp", None);
        assert_eq!(pkt.unwrap().platform, current_platform());
        assert_eq!(peer.unwrap().platform, current_platform());

        let props = build_mdns_txt_properties("dev-os", "Host", "trusted-lan", current_platform());
        assert!(props.contains(&("platform", current_platform())));
    }

    #[test]
    fn test_bind_udp_reusable_ephemeral_socket() {
        let sock = bind_udp_reusable(0).expect("ephemeral UDP bind should succeed");
        assert!(sock.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn test_beacon_and_mdns_v3_optional_fields() {
        let meta = BeaconV3Meta {
            v3_port: Some(4714),
            pair_port: Some(4715),
            pub_fp: Some("A1B2-C3D4-E5F6-0708".to_string()),
            caps: vec!["sht3".to_string(), "opus".to_string(), "pcm".to_string()],
        };
        let key = "0123456789abcdef0123456789abcdef";
        let raw = make_beacon_v3(
            "dev-v3",
            "Studio V3",
            4713,
            "psk",
            "offer",
            Some(key),
            None,
            None,
            &meta,
        );
        let (pkt, peer) = parse_beacon(&raw, "192.168.1.55", "udp", Some(key));
        let pkt = pkt.unwrap();
        assert_eq!(pkt.v3_port, Some(4714));
        assert_eq!(pkt.pair_port, Some(4715));
        assert_eq!(pkt.pub_fp.as_deref(), Some("A1B2-C3D4-E5F6-0708"));
        assert_eq!(pkt.caps, vec!["sht3", "opus", "pcm"]);
        assert!(peer.is_some());

        // Tampering with pair_port must invalidate HMAC
        let mut tampered = pkt.clone();
        tampered.pair_port = Some(9999);
        let tampered_raw = serde_json::to_vec(&tampered).unwrap();
        assert!(
            parse_beacon(&tampered_raw, "192.168.1.55", "udp", Some(key))
                .1
                .is_none()
        );

        // mDNS TXT v3 roundtrip
        let props = build_mdns_txt_properties_v3("dev-v3", "Studio V3", "psk", "linux", &meta);
        let prop_refs: Vec<(&str, &str)> = props.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            "studio-v3",
            "studio-v3.local.",
            "10.254.254.55",
            4713,
            &prop_refs[..],
        )
        .unwrap();
        let parsed_meta = parse_mdns_v3_metadata(&info);
        assert_eq!(parsed_meta, meta);
    }

    #[test]
    fn test_set_advertising_unregisters_and_reregisters_mdns() {
        let mgr = DiscoveryManager::new(
            "dev-mdns-toggle".to_string(),
            "Toggle Rig".to_string(),
            4713,
            47777,
            "trusted-lan".to_string(),
            None,
            5.0,
            false,
            true,
        );
        let _warnings = mgr.start(true);
        let has_daemon = lock_or_recover(&mgr.mdns_daemon).is_some();
        if has_daemon {
            let initial_fullname = lock_or_recover(&mgr.mdns_fullname).clone();
            assert!(
                initial_fullname.is_some(),
                "start(true) should record mdns_fullname when mDNS daemon starts"
            );

            mgr.set_advertising(false);
            assert!(
                lock_or_recover(&mgr.mdns_fullname).is_none(),
                "set_advertising(false) should clear mdns_fullname"
            );
            assert!(!mgr.advertising.load(Ordering::Relaxed));

            mgr.set_advertising(true);
            assert_eq!(
                lock_or_recover(&mgr.mdns_fullname).clone(),
                initial_fullname,
                "set_advertising(true) should re-register and restore mdns_fullname"
            );
            assert!(mgr.advertising.load(Ordering::Relaxed));
        }
        mgr.stop();
        assert!(lock_or_recover(&mgr.mdns_fullname).is_none());
    }
}
