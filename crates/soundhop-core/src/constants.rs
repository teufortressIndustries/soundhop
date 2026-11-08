//! Shared constants for SoundHop v2.0.

pub const APP_NAME: &str = "SoundHop";
pub const APP_SLUG: &str = "soundhop";
pub const APP_ID: &str = "io.github.soundhop";
pub const VERSION: &str = "2.0.0";
pub const REPO_URL: &str = "https://github.com/teufortressIndustries/soundhop";
pub const ISSUES_URL: &str = "https://github.com/teufortressIndustries/soundhop/issues";

/// Version of the on-the-wire discovery and control protocol (v2 in Rust rewrite).
pub const PROTOCOL_VERSION: u32 = 2;

/// PulseAudio native protocol TCP port used for audio streaming.
pub const DEFAULT_AUDIO_PORT: u16 = 4713;

/// UDP port used by SoundHop's zero-configuration LAN discovery.
pub const DEFAULT_DISCOVERY_PORT: u16 = 47777;

/// Standard mDNS UDP port.
pub const MDNS_PORT: u16 = 5353;

/// mDNS service type registered via `mdns-sd`.
pub const MDNS_SERVICE_TYPE: &str = "_soundhop._tcp.local.";

/// Prefix for PulseAudio/PipeWire modules, sinks, and sources created by SoundHop.
pub const MODULE_PREFIX: &str = "soundhop_";
pub const SINK_PREFIX: &str = "soundhop_";
pub const SOURCE_PREFIX: &str = "soundhop_source_";
pub const MULTIROOM_SINK_NAME: &str = "soundhop_multiroom";

/// How often a daemon announces itself, and how long a peer is considered online.
pub const BEACON_INTERVAL_SECS: f64 = 3.0;
pub const PEER_TIMEOUT_SECS: f64 = 20.0;

/// How long to wait for a new tunnel sink or source to appear in the graph.
pub const SINK_TIMEOUT_SECS: f64 = 3.0;
pub const SOURCE_TIMEOUT_SECS: f64 = 3.0;

/// How often the daemon checks whether it should auto-connect/reconnect.
pub const SUPERVISOR_INTERVAL_SECS: f64 = 2.0;

/// Number of failed TCP probes before an auto-connected peer is dropped.
pub const OFFLINE_PROBE_LIMIT: u32 = 3;

/// Default private networks that may connect in `trusted-lan` mode.
pub const DEFAULT_LAN_NETWORKS: &[&str] = &[
    "127.0.0.1",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "fc00::/7",
    "::1",
];

/// Supported listener security modes.
pub const SECURITY_MODES: &[&str] = &["trusted-lan", "psk", "pin", "cookie", "open"];

/// Supported Android scrcpy audio codecs.
pub const SUPPORTED_AUDIO_CODECS: &[&str] = &["opus", "aac", "flac", "raw"];
pub const DEFAULT_AUDIO_CODEC: &str = "aac";
pub const DEFAULT_AUDIO_BIT_RATE: u32 = 128_000;
pub const DEFAULT_AUDIO_BUFFER_MS: u32 = 50;
pub const DEFAULT_SCRCPY_VERSION: &str = "5.0.1";
